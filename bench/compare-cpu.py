#!/usr/bin/env python3
"""Bounded, interleaved translator A/B runs. Run on the Intel Mac; caches are disabled."""
import argparse
import json
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("baseline")
p.add_argument("optimized")
p.add_argument("benchmark")
p.add_argument("--sysroot", required=True)
p.add_argument("--rounds", type=int, default=5)
p.add_argument("--seconds", type=int, default=20)
p.add_argument("--output", type=Path, required=True)
args = p.parse_args()
if not 1 <= args.rounds <= 50 or not 1 <= args.seconds <= 60:
    p.error("Use 1..50 rounds and a 1..60 second per-process deadline")
args.output.parent.mkdir(parents=True, exist_ok=True)
runs = []
for n in range(args.rounds):
    order = ["baseline", "optimized"] if n % 2 == 0 else ["optimized", "baseline"]
    for label in order:
        cmd = ["/usr/bin/nice", "-n", "10", getattr(args, label), "--no-aot",
               "--sysroot", args.sysroot, args.benchmark]
        log_path = args.output.with_name(f"{args.output.stem}-{n + 1}-{label}.txt")
        start = time.monotonic()
        timed_out = False
        with log_path.open("w") as log:
            proc = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT,
                                    start_new_session=True)
            try:
                code = proc.wait(args.seconds)
            except subprocess.TimeoutExpired:
                timed_out = True
                os.killpg(proc.pid, signal.SIGTERM)
                try:
                    code = proc.wait(5)
                except subprocess.TimeoutExpired:
                    os.killpg(proc.pid, signal.SIGKILL)
                    code = proc.wait()
        text = log_path.read_text(errors="replace")
        timings = {m[1].strip(): float(m[2]) for m in
                   re.finditer(r"^(.+?)\s+([\d.]+) ms(?:\s.*)?$", text, re.M)}
        checksum = re.search(r"checksum=(\d+) expected=(\d+)", text)
        valid = code == 0 and not timed_out and bool(timings)
        if checksum:
            valid = valid and checksum[1] == checksum[2]
        record = dict(round=n + 1, label=label, exit_code=code, valid=valid,
                      timed_out=timed_out, elapsed_seconds=round(time.monotonic() - start, 3),
                      timings_ms=timings, log=str(log_path))
        runs.append(record)
        print(json.dumps(record), flush=True)
        if not valid:
            args.output.write_text(json.dumps({"runs": runs}, indent=2) + "\n")
            raise SystemExit("Benchmark failed: inspect the saved log")
summary = {}
for name in runs[0]["timings_ms"]:
    med = {label: statistics.median([r["timings_ms"][name] for r in runs if r["label"] == label])
           for label in ["baseline", "optimized"]}
    med["time_reduction_percent"] = round(100 * (1 - med["optimized"] / med["baseline"]), 2)
    summary[name] = med
args.output.write_text(json.dumps({"runs": runs, "median_timings_ms": summary}, indent=2) + "\n")
print(json.dumps(summary, indent=2), flush=True)
