#!/usr/bin/env python3
"""Run real ARM signal fixtures, with a deadline for every process group."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time

ALL = ["basic", "mask", "suspend", "altstack", "registers", "nested", "reset", "read", "restart", "worker", "alarm", "invalid"]
p = argparse.ArgumentParser()
p.add_argument("--runtime", required=True)
p.add_argument("--guest", required=True)
p.add_argument("--sysroot", required=True)
p.add_argument("--output", required=True)
p.add_argument("--cases", nargs="+", default=ALL)
p.add_argument("--interp", action="store_true")
p.add_argument("--deadline", type=float, default=12)
a = p.parse_args()
output = Path(a.output)
output.mkdir(parents=True, exist_ok=True)
results = []
for case in a.cases:
    command = [a.runtime, "--interp" if a.interp else "--no-aot", "--sysroot", a.sysroot, a.guest, case]
    environment = dict(os.environ, MACLATOR_JIT_THRESHOLD="0")
    started = time.monotonic()
    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, start_new_session=True, env=environment)
    timed_out = False
    try:
        raw, _ = process.communicate(timeout=a.deadline)
    except subprocess.TimeoutExpired:
        timed_out = True
        os.killpg(process.pid, signal.SIGKILL)
        raw, _ = process.communicate(timeout=3)
    text = raw.decode("utf-8", errors="replace")
    success = not timed_out and process.returncode == 0 and f"PASS: {case} " in text
    result = dict(case=case, command=command, returncode=process.returncode, timeout=timed_out, seconds=time.monotonic()-started, success=success)
    results.append(result)
    (output / f"{case}.txt").write_text(text)
    print(json.dumps(result, ensure_ascii=False), flush=True)
    if not success:
        print(text[-1600:], flush=True)
(output / "results.json").write_text(json.dumps(results, indent=2))
raise SystemExit(0 if all(result["success"] for result in results) else 1)
