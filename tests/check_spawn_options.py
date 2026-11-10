"""Verify guest child status and --no-aot propagation against a built Maclator."""
import argparse
import os
from pathlib import Path
import signal
import subprocess
import threading

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runtime", type=Path)
parser.add_argument("--guest", type=Path, default=Path(__file__).resolve().parent / "guest" / "spawn_options")
parser.add_argument("--sysroot", type=Path)
args = parser.parse_args()
runtime = args.runtime.resolve()
guest = args.guest.resolve()
command = [str(runtime), "--no-aot"]
if args.sysroot:
    command.extend(["--sysroot", str(args.sysroot.resolve())])
command.append(str(guest))
process = subprocess.Popen(
    command,
    stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    start_new_session=True,
)

def stop():
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

deadline = threading.Timer(15, stop)
deadline.start()
try:
    line = process.stdout.readline().strip()
    if not line.startswith("child="):
        raise RuntimeError(f"guest did not report its child: {line!r}")
    child = int(line.removeprefix("child="))
    command = subprocess.check_output(
        ["/bin/ps", "-p", str(child), "-o", "command="], text=True,
    ).strip()
    print(command, flush=True)
    if "--no-aot" not in command.split():
        raise RuntimeError("guest child did not inherit --no-aot")
    stdout, stderr = process.communicate(timeout=8)
    print(stdout, end="")
    if process.returncode != 0:
        raise RuntimeError(f"guest exit={process.returncode}: {stderr}")
    print("PASS: child inherited --no-aot")
finally:
    deadline.cancel()
    stop()
    process.wait()
