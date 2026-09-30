import socket, base64, os, struct, json, time, urllib.request, sys

def ws_connect(url):
    assert url.startswith("ws://")
    hostport, path = url[5:].split("/", 1)
    host, port = hostport.split(":")
    s = socket.create_connection((host, int(port)))
    key = base64.b64encode(os.urandom(16)).decode()
    s.sendall((f"GET /{path} HTTP/1.1\r\nHost: {hostport}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
               f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n").encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += s.recv(4096)
    return s

def send(s, obj):
    data = json.dumps(obj).encode()
    hdr = bytearray([0x81])
    n = len(data)
    if n < 126: hdr.append(0x80 | n)
    elif n < 65536: hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
    else: hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
    mask = os.urandom(4)
    s.sendall(bytes(hdr) + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

def recv_exact(s, n):
    b = b""
    while len(b) < n:
        c = s.recv(n - len(b))
        if not c: raise EOFError
        b += c
    return b

def recv(s):
    msg = b""
    while True:
        h = recv_exact(s, 2)
        op, ln = h[0] & 0x0f, h[1] & 0x7f
        if ln == 126: ln = struct.unpack(">H", recv_exact(s, 2))[0]
        elif ln == 127: ln = struct.unpack(">Q", recv_exact(s, 8))[0]
        msg += recv_exact(s, ln)
        if h[0] & 0x80: break
    return json.loads(msg) if msg else {}

_id = 0
def call(s, method, params=None):
    global _id
    _id += 1
    send(s, {"id": _id, "method": method, "params": params or {}})
    while True:
        r = recv(s)
        if r.get("id") == _id: return r

def ev_await(s, expr):
    r = call(s, "Runtime.evaluate", {"expression": expr, "returnByValue": True, "awaitPromise": True})
    return r.get("result", {}).get("result", {}).get("value")

def ev(s, expr):
    r = call(s, "Runtime.evaluate", {"expression": expr, "returnByValue": True})
    return r.get("result", {}).get("result", {}).get("value")

port = sys.argv[1] if len(sys.argv) > 1 else "9222"
start_epoch = float(sys.argv[2]) if len(sys.argv) > 2 else time.time()
targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
page = [t for t in targets if t.get("type") == "page" and "workbench" in t.get("url", "")][0]
s = ws_connect(page["webSocketDebuggerUrl"])
print("connected:", page["title"][:60])

import subprocess
def cpu_total():
    out = subprocess.run("ps -axo pcpu,command | grep '[m]aclator' | grep -v 'zsh -c' | awk '{s+=$1} END {print s+0}'", shell=True, capture_output=True, text=True).stdout
    return float(out.strip() or 0)
# wait for the workbench, then for the machine to go idle
for _ in range(120):
    if ev(s, "!!document.querySelector('.monaco-workbench')"): break
    time.sleep(1)
t_wb = time.time() - start_epoch
print("WORKBENCH DOM present at %.0fs" % t_wb)
calm = 0
while time.time() - start_epoch < 420:
    c = cpu_total()
    calm = calm + 1 if c < 40 else 0
    if calm >= 4: break
    time.sleep(2)
print("IDLE (cpu<40%% for 8s) reached at %.0fs" % (time.time() - start_epoch))
for typ in ("mousePressed", "mouseReleased"):
    call(s, "Input.dispatchMouseEvent", {"type": typ, "x": 500, "y": 400, "button": "left", "clickCount": 1})
for typ in ("rawKeyDown", "keyUp"):
    call(s, "Input.dispatchKeyEvent", {"type": typ, "modifiers": 4, "key": "n", "code": "KeyN", "windowsVirtualKeyCode": 78, "nativeVirtualKeyCode": 45})
t0 = time.time()
while not ev(s, "!!document.querySelector('.monaco-editor .view-lines')"):
    time.sleep(0.3)
    if time.time() - t0 > 90: print("no editor"); sys.exit(1)
print("editor opened in %.1fs" % (time.time() - t0))
time.sleep(2)
js = "(()=>{const t=performance.now(); let s=0; for(let i=0;i<3e6;i++) s+=i%7; const t1=performance.now(); const d=document.createElement('div'); for(let i=0;i<1500;i++){const e=document.createElement('span'); e.textContent='x'+i; d.appendChild(e);} document.body.appendChild(d); void d.offsetHeight; const t2=performance.now(); d.remove(); return [Math.round(t1-t), Math.round(t2-t1)];})()"
r = ev(s, js)
print("IN-PAGE js 3e6-loop = %d ms, DOM 1500 spans+layout = %d ms" % (r[0], r[1]))
FRAME_JS = "new Promise(r=>{const t=performance.now(); requestAnimationFrame(()=>requestAnimationFrame(()=>r(performance.now()-t)))})"
idle = sorted(ev_await(s, FRAME_JS) for _ in range(8))
print("FRAME idle 2xrAF ms: median=%.0f max=%.0f" % (idle[len(idle)//2], idle[-1]))
text = "the quick brown fox jumps over the lazy dog" * int(os.environ.get("TYPE_REPEAT", "1"))
lat = []
for ch in text:
    t = time.time()
    call(s, "Input.dispatchKeyEvent", {"type": "keyDown", "text": ch, "key": ch, "unmodifiedText": ch})
    call(s, "Input.dispatchKeyEvent", {"type": "keyUp", "key": ch})
    lat.append((time.time() - t) * 1000)
lat.sort()
frames = []
for ch in "abcdefghijklmnopqrst":
    call(s, "Input.dispatchKeyEvent", {"type": "keyDown", "text": ch, "key": ch, "unmodifiedText": ch})
    call(s, "Input.dispatchKeyEvent", {"type": "keyUp", "key": ch})
    frames.append(ev_await(s, FRAME_JS))
frames.sort()
print("FRAME after key 2xrAF ms: avg=%.0f median=%.0f p90=%.0f max=%.0f" % (sum(frames)/len(frames), frames[len(frames)//2], frames[int(len(frames)*0.9)], frames[-1]))
print("KEY dispatch latency ms: avg=%.0f median=%.0f p90=%.0f max=%.0f" % (sum(lat)/len(lat), lat[len(lat)//2], lat[int(len(lat)*0.9)], lat[-1]))
