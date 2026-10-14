#!/usr/bin/env python3
"""Reversible local lifecycle acceptance. Own software + dedicated sleep only."""
import argparse
import hashlib
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
if args.uid is None or args.uid <= 0:
    raise SystemExit("pass --uid for the logged-in user, or invoke through sudo")
account = pwd.getpwuid(args.uid)
SOURCE = ROOT / "dist/MacFlow.app"
CLI = SOURCE / "Contents/MacOS/macflow"
APP = Path("/Applications/MacFlow.app")
RUNTIME = Path("/Library/Application Support/MacFlow/Runtime")
report = {"steps": []}


def run(command):
    result = subprocess.run([str(x) for x in command], capture_output=True, text=True, timeout=35)
    if result.returncode:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def user(*args):
    return run(["/bin/launchctl", "asuser", str(account.pw_uid), "/usr/bin/sudo", "-H", "-u", f"#{account.pw_uid}", "--", APP / "Contents/MacOS/macflow", *args])


def status():
    for _ in range(30):
        try:
            value = json.loads(user("status", "--json"))
            if value["helper_connected"]:
                assert value["active_leases"] == 0
                return value
        except RuntimeError:
            pass
        time.sleep(0.1)
    raise RuntimeError("installed services failed to become ready")


def install():
    run([CLI, "install-root", "--app", SOURCE, "--uid", str(account.pw_uid)])
    user("register-services")
    status()


if os.geteuid() != 0:
    raise SystemExit("requires root for this explicit lifecycle test")
child = None
try:
    install()
    latest = hashlib.sha256((APP / "Contents/MacOS/macflow-agent").read_bytes()).hexdigest()
    assert latest == hashlib.sha256((SOURCE / "Contents/MacOS/macflow-agent").read_bytes()).hexdigest()
    report["steps"].append({"action": "update", "installed_hash_verified": True})
    run([RUNTIME / "macflow-control", "rollback-root"])
    user("register-services")
    status()
    rolled = hashlib.sha256((APP / "Contents/MacOS/macflow-agent").read_bytes()).hexdigest()
    report["steps"].append({"action": "rollback", "previous_software_restored": rolled != latest})
    assert rolled != latest
    install()
    child = subprocess.Popen(["/bin/launchctl", "asuser", str(account.pw_uid), "/usr/bin/sudo", "-H", "-u", f"#{account.pw_uid}", "--", str(APP / "Contents/MacOS/macflow-agent"), "--helper-hold-smoke"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    holding = json.loads(child.stdout.readline())
    assert os.getpriority(os.PRIO_PROCESS, holding["pid"]) == holding["applied_nice"]
    run([RUNTIME / "macflow-control", "uninstall-root"])
    stdout, stderr = child.communicate(timeout=20)
    assert child.returncode == 0, stderr
    completed = json.loads(stdout.strip().splitlines()[-1])
    assert completed["restored"]
    assert not APP.exists()
    assert not (RUNTIME / "macflow-helper").exists()
    for domain in ["system/org.macflow.helper", f"gui/{account.pw_uid}/org.macflow.agent"]:
        assert subprocess.run(["/bin/launchctl", "print", domain], capture_output=True).returncode != 0
    report["steps"].append({"action": "uninstall_with_live_test_lease", "lease_restored": completed, "app_removed": True, "services_removed": True})
    install()
    user("observe", "on")
    user("resume")
    final = status()
    report["steps"].append({"action": "reinstall", "helper_connected": final["helper_connected"], "active_leases": final["active_leases"], "config": final["config"]})
except BaseException as error:
    report["error"] = str(error)
    # Keep the requested software available even if a reversible test fails.
    try:
        install()
        user("observe", "on")
        user("resume")
        report["recovery_reinstall"] = True
    except BaseException as recovery:
        report["recovery_error"] = str(recovery)
    raise
finally:
    if child is not None and child.poll() is None:
        child.terminate()
        child.communicate(timeout=5)
    output = ROOT / "validation/lifecycle-integration.json"
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(report, indent=2) + "\n")
    os.chown(output, account.pw_uid, account.pw_gid)
    print(json.dumps(report))
