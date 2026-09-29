#!/bin/bash
# usage: cfg.sh <label> <script> "<extra chromium flags>"
H=vihaannathan@vihaans-mac-pro.local
scp -q cfgbench.py $H:maclator-run/
ssh $H "pkill -f '[m]aclator.*Chromium'; sleep 2; rm -f ~/gpu.log; t0=\$(date +%s); (nohup ~/bin/$2 --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling $3 about:blank >~/gpu.log 2>&1 &); for k in \$(seq 1 120); do curl -s -m 1 http://127.0.0.1:9333/json/version >/dev/null && break; sleep 1; done; echo \"$1: devtools up in \$((\$(date +%s)-t0))s\"; python3 ~/maclator-run/cfgbench.py; command grep -c 'GPU process exited' ~/gpu.log"
