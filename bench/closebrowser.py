import sys, json, urllib.request
sys.path.insert(0, "/Users/vihaannathan/maclator-run")
from ws import *
v = json.load(urllib.request.urlopen("http://127.0.0.1:9333/json/version"))
s = ws_connect(v["webSocketDebuggerUrl"])
try:
    call(s, "Browser.close")
except Exception as e:
    pass
print("closed")
