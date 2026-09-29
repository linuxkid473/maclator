import sys, json, time, urllib.request, urllib.parse, subprocess
sys.path.insert(0, "/Users/vihaannathan/maclator-run")
from ws import *
def idle_stats():
    out = subprocess.run("ps -Ao pcpu,rss,command | command grep '[m]aclator --dyld\\|[m]aclator --gpu' | awk '{c+=$1; r+=$2; n++} END {print n, c, int(r/1024)}'", shell=True, capture_output=True, text=True, executable="/bin/zsh").stdout.split()
    return dict(procs=int(out[0]), cpu=float(out[1]), rss_mb=int(out[2])) if len(out) == 3 else {}
tabs = json.load(urllib.request.urlopen("http://127.0.0.1:9333/json"))
t = [t for t in tabs if t["type"] == "page"][0]
s = ws_connect(t["webSocketDebuggerUrl"])
html = "<style>div{padding:2px;border:1px solid #ccc;margin:1px;font:12px sans-serif}</style><body></body><script>for(var i=0;i<4000;i++){var d=document.createElement('div');d.textContent='row '+i+' '+Math.sin(i);document.body.appendChild(d)}document.title=document.body.scrollHeight</script>"
call(s, "Page.enable")
t0 = time.time()
call(s, "Page.navigate", {"url": "data:text/html," + urllib.parse.quote(html)})
for _ in range(600):
    time.sleep(0.5)
    v = ev(s, "document.readyState + ' ' + performance.timing.loadEventEnd")
    if v and v.startswith("complete") and not v.endswith(" 0"):
        break
load_s = time.time() - t0
lt = ev(s, "performance.timing.loadEventEnd - performance.timing.navigationStart")
time.sleep(12)
res = dict(load_wall_s=round(load_s, 1), load_ms=lt)
res.update(idle_stats())
js = "(function(){var t=performance.now();function fib(n){return n<2?n:fib(n-1)+fib(n-2)}fib(27);var a=[];for(var i=0;i<300000;i++)a.push((i*7919)%10007);a.sort(function(x,y){return x-y});var s=0;for(var i=0;i<3e6;i++){s+=i%7;if(s>1e9)s=0}return Math.round(performance.now()-t)})()"
res["js_ms"] = [ev(s, js) for _ in range(3)]
print(json.dumps(res))
