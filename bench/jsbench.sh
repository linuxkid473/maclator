#!/bin/bash
# usage: jsb.sh <binary suffix in ~/bin/maclator.<suffix>>  -- runs the canvas+JS bench on that maclator build
H=${HACK:?set HACK=user@your-intel-mac.local}
ssh $H "pkill -f '[m]aclator.*Chromium'; sleep 2; cp ~/bin/maclator.$1 ~/bin/maclator; rm -f ~/gpu.log; (nohup ~/bin/chromium-gpu --disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-background-timer-throttling about:blank >~/gpu.log 2>&1 &); sleep 40; echo started $1" >/dev/null
ssh $H "python3 ~/maclator-run/gpubench.py 15 200 js"
