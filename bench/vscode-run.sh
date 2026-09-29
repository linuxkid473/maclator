#!/bin/bash
# usage: vscode_run.sh   -- launch VS Code on the hack, run typebench, close gracefully (builds the AOT cache)
H=vihaannathan@vihaans-mac-pro.local
ssh $H 'pkill -f "[m]aclator.*Code"; pkill -f "[m]aclator.*Chromium"; sleep 3; cd ~/vscode-arm64 && (nohup ~/bin/maclator "./Visual Studio Code.app/Contents/MacOS/Code" --no-sandbox --disable-gpu --js-flags=--jitless --user-data-dir=$HOME/vscode-data --disable-workspace-trust --remote-debugging-port=9222 >~/vscode.log 2>&1 &); sleep 2; echo launched'
ssh $H 'perl -e "alarm 560; exec @ARGV" python3 ~/maclator-run/typebench.py 9222 $(date +%s) 2>&1 | tail -12'
scp -q "$(dirname "$0")/closebrowser.py" $H:maclator-run/closebrowser9222.py 2>/dev/null
ssh $H 'sed -i "" "s/9333/9222/" ~/maclator-run/closebrowser9222.py; python3 ~/maclator-run/closebrowser9222.py; for i in $(seq 1 120); do n=$(pgrep -f "[m]aclator" | wc -l); [ "$n" -eq 0 ] && break; sleep 3; done; echo remaining=$(pgrep -f "[m]aclator" | wc -l)'
