#!/usr/bin/env python3
"""Measure same-host native/translated ratios; never equate JIT coverage with speed."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import time


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--native", required=True)
    p.add_argument("--guest", required=True)
    p.add_argument("--baseline", required=True)
    p.add_argument("--optimized")
    p.add_argument("--variants", type=Path,
                   help="JSON mapping of additional labels to runtime path and environment overrides")
    p.add_argument("--sysroot", required=True)
    p.add_argument("--rounds", type=int, default=7)
    p.add_argument("--deadline", type=int, default=30)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    if not 3 <= a.rounds <= 30 or not 1 <= a.deadline <= 60:
        p.error("Use 3..30 rounds and a 1..60 second deadline")
    a.output.parent.mkdir(parents=True, exist_ok=True)
    commands = {"native": [a.native], "baseline": [a.baseline, "--no-aot", "--sysroot", a.sysroot, a.guest]}
    if a.optimized:
        commands["optimized"] = [a.optimized, "--no-aot", "--sysroot", a.sysroot, a.guest]
    overrides = {name: {} for name in commands}
    if a.variants:
        variants = json.loads(a.variants.read_text())
        if not isinstance(variants, dict):
            p.error("Variants must be a JSON object")
        for name, spec in variants.items():
            if not re.fullmatch(r"[A-Za-z0-9_-]+", name) or name in commands:
                p.error(f"Invalid or duplicate variant label: {name}")
            if not isinstance(spec, dict) or not isinstance(spec.get("runtime"), str):
                p.error(f"Variant {name} requires a runtime path")
            env = spec.get("environment", {})
            if not isinstance(env, dict) or any(not k.startswith("MACLATOR_") or not isinstance(v, str)
                                                for k, v in env.items()):
                p.error(f"Variant {name}: only string MACLATOR_ environment overrides are allowed")
            commands[name] = [spec["runtime"], "--no-aot", "--sysroot", a.sysroot, a.guest]
            overrides[name] = env
    names = list(commands)
    paths = list(dict.fromkeys([a.native, a.guest] + [command[0] for command in commands.values()]))
    hashes_before = {path: hashlib.sha256(Path(path).read_bytes()).hexdigest() for path in paths}
    started_at_unix_s = time.time()
    runs = []
    checksum = None
    for n in range(a.rounds + 1):  # A whole interleaved round is warmup, saved but excluded.
        order = names[n % len(names):] + names[:n % len(names)]
        if n % 2:
            order.reverse()
        for name in order:
            log = a.output.with_name(a.output.stem + f"-{n}-{name}.txt")
            start = time.monotonic()
            with log.open("w") as stream:
                child = subprocess.Popen(["/usr/bin/nice", "-n", "10"] + commands[name],
                    stdout=stream, stderr=subprocess.STDOUT, start_new_session=True,
                    env=dict(os.environ, **overrides[name]))
                try:
                    code = child.wait(timeout=a.deadline)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
                    raise RuntimeError(f"{name} exceeded {a.deadline}s: {log}")
            text = log.read_text(errors="replace")
            timings = {m[1].strip(): float(m[2]) for m in re.finditer(r"^(.+?)\s+([\d.]+) ms$", text, re.M)}
            match = re.search(r"^checksum=(\d+)$", text, re.M)
            if code != 0 or not match or not timings or any(v <= 0 for v in timings.values()):
                raise RuntimeError(f"invalid benchmark {name}: exit={code}, log={log}")
            current = int(match[1])
            if checksum is None:
                checksum = current
            if current != checksum:
                raise RuntimeError(f"checksum differs: {current} != {checksum}")
            run = dict(round=n, label=name, warmup=n == 0, elapsed_s=time.monotonic() - start,
                       completed_at_unix_s=time.time(), timings_ms=timings,
                       checksum=current, exit_code=code, log=str(log))
            runs.append(run)
            a.output.write_text(json.dumps(dict(commands=commands, environment_overrides=overrides,
                                                sha256_before=hashes_before,
                                                started_at_unix_s=started_at_unix_s,
                                                runs=runs), indent=2) + "\n")
            print(f"round {n} {name}: {timings['TOTAL']:.3f} ms", flush=True)
    valid = [r for r in runs if not r["warmup"]]
    medians = {name: {key: statistics.median(r["timings_ms"][key] for r in valid if r["label"] == name)
                         for key in valid[0]["timings_ms"]} for name in names}
    ratios = {}
    for name in names[1:]:
        per_kernel = {key: medians["native"][key] / medians[name][key] for key in medians[name] if key != "TOTAL"}
        ratios[name] = dict(per_kernel=per_kernel,
            geometric_mean=math.exp(statistics.mean(math.log(v) for v in per_kernel.values())),
            total_ratio=medians["native"]["TOTAL"] / medians[name]["TOTAL"],
            worst_ratio=min(per_kernel.values()), target=0.95,
            all_kernels_reach_target=all(v >= 0.95 for v in per_kernel.values()))
    hashes = {path: hashlib.sha256(Path(path).read_bytes()).hexdigest() for path in paths}
    if hashes != hashes_before:
        a.output.write_text(json.dumps(dict(commands=commands, environment_overrides=overrides,
            runs=runs, sha256_before=hashes_before, sha256_after=hashes,
            started_at_unix_s=started_at_unix_s, inputs_unchanged=False), indent=2) + "\n")
        raise RuntimeError("Benchmark input changed during measurement; ratios are not accepted")
    result = dict(commands=commands, runs=runs, median_timings_ms=medians, native_ratios=ratios,
                  checksum=checksum, sha256=hashes, environment_overrides=overrides,
                  sha256_before=hashes_before, inputs_unchanged=True,
                  started_at_unix_s=started_at_unix_s,
                  definition="native time / translated time; same hardware, -O2; warmup excluded")
    a.output.write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(ratios, indent=2), flush=True)


if __name__ == "__main__":
    main()
