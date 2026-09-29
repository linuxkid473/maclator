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
