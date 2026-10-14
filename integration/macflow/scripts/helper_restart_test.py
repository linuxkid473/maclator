#!/usr/bin/env python3
"""Root integration check. Restarts only MacFlow's own helper, on a sleep child."""
import argparse
import json
import os
from pathlib import Path
import pwd
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--uid", type=int, default=os.environ.get("SUDO_UID"),
                    help="logged-in user's UID; defaults to SUDO_UID")
args = parser.parse_args()
if os.geteuid() != 0:
    raise SystemExit("requires root for restarting the MacFlow helper")
if args.uid is None or args.uid <= 0:
    raise SystemExit("pass --uid for the logged-in user, or invoke through sudo")
user = pwd.getpwuid(args.uid)
process = subprocess.Popen(["/bin/launchctl", "asuser", str(args.uid), "/usr/bin/sudo", "-H", "-u", f"#{args.uid}", "--", "/Applications/MacFlow.app/Contents/MacOS/macflow-agent", "--helper-hold-smoke"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
try:
    holding = json.loads(process.stdout.readline())
    assert holding["phase"] == "holding"
    before = os.getpriority(os.PRIO_PROCESS, holding["pid"])
    assert before == holding["applied_nice"]
    started = time.monotonic()
    subprocess.run(["/bin/launchctl", "kickstart", "-k", "system/org.macflow.helper"], check=True, timeout=5)
    while time.monotonic() - started < 5:
        if os.getpriority(os.PRIO_PROCESS, holding["pid"]) == holding["original_nice"]:
            break
        time.sleep(0.1)
    else:
        raise RuntimeError("helper restart did not restore within five seconds")
    restored_seconds = time.monotonic() - started
    stdout, stderr = process.communicate(timeout=20)
    assert process.returncode == 0, stderr
    completed = json.loads(stdout.strip().splitlines()[-1])
    report = {"holding": holding, "completed": completed, "restart_restore_seconds": restored_seconds, "actual_nice_before_restart": before, "journal_recovery_verified_before_lease_expiry": completed["restored_after_ms"] < 5000}
    assert report["journal_recovery_verified_before_lease_expiry"]
    output = ROOT / "validation/helper-restart-verified.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    os.chown(output, args.uid, user.pw_gid)
    print(json.dumps(report))
finally:
    if process.poll() is None:
        process.terminate()
        process.communicate(timeout=5)
