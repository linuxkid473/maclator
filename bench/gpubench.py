import sys, json, time, urllib.request
from ws import *
import urllib.parse
HTML = """<canvas id=c width=800 height=500></canvas><div id=o style='position:fixed;top:0;left:0;background:#fff'></div><style>body{margin:0}canvas{transform:rotate(0deg)}</style><script>
var N=+(location.hash.slice(1)||60),c=document.getElementById('c'),x=c.getContext('2d'),t=0,ft=[],last=performance.now();
function f(n){ft.push(n-last);last=n;t++;x.clearRect(0,0,800,500);for(var i=0;i<N;i++){x.fillStyle='hsl('+((i*6+t*3)%360)+',80%,50%)';x.beginPath();x.arc(400+Math.cos(t/20+i)*300,250+Math.sin(t/17+i)*200,40,0,7);x.fill();}c.style.transform='rotate('+t/4+'deg)';requestAnimationFrame(f)}requestAnimationFrame(f);
window.stats=function(){var a=ft.slice(-60).sort(function(a,b){return a-b});return JSON.stringify({n:ft.length,med:a[a.length>>1],fps:1000/(a.reduce(function(s,v){return s+v},0)/a.length)})}
</script>"""
PAGE = "data:text/html," + urllib.parse.quote(HTML) + "#" + (sys.argv[2] if len(sys.argv) > 2 else "60")
dur = int(sys.argv[1]) if len(sys.argv) > 1 else 40
for _ in range(60):
    try:
        tabs = json.load(urllib.request.urlopen("http://127.0.0.1:9333/json")); break
    except Exception: time.sleep(2)
t = [t for t in tabs if t["type"] == "page"][0]
s = ws_connect(t["webSocketDebuggerUrl"])
call(s, "Emulation.setFocusEmulationEnabled", {"enabled": True})
call(s, "Page.bringToFront")
call(s, "Page.navigate", {"url": PAGE})
t0 = time.time(); res = None
while time.time() - t0 < 150:
    time.sleep(5)
    try:
        r = json.loads(ev(s, "window.stats ? window.stats() : 'null'") or "null")
    except Exception as e:
        r = None
    if r and r.get("n", 0) >= 40 and time.time() - t0 >= dur:
        res = r; break
print(json.dumps(res) if res else "NO FRAMES", "after %ds" % (time.time() - t0))

if len(sys.argv) > 3 and sys.argv[3] == "js":
    js = "(function(){var t=performance.now();function fib(n){return n<2?n:fib(n-1)+fib(n-2)}fib(27);var a=[];for(var i=0;i<300000;i++)a.push((i*7919)%10007);a.sort(function(x,y){return x-y});var s=0;for(var i=0;i<3e6;i++){s+=i%7;if(s>1e9)s=0}return Math.round(performance.now()-t)})()"
    print("js_ms", [ev(s, js) for _ in range(4)])
