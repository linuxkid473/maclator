#!/bin/bash
# usage: exp.sh "ENV=1 ENV2=1" [bench-seconds]
H=vihaannathan@vihaans-mac-pro.local
ssh $H "pkill -f \"[m]aclator.*Chromium\"; sleep 2; rm -f ~/gpu.log; (env MCL_STATS=1 $1 nohup ~/bin/${CG:-chromium-gpu} --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling about:blank >~/gpu.log 2>&1 &); sleep 42; echo up" >/dev/null
scp -q gpubench.py ws.py $H:maclator-run/
ssh $H "python3 ~/maclator-run/gpubench.py ${2:-30}"
ssh $H 'command grep -c "GPU process exited" ~/gpu.log; command grep "mclbridge\]" ~/gpu.log | sort | uniq -c | head -5; command grep "mclguest" ~/gpu.log | tail -2 | cut -c1-260'
