#!/usr/bin/env python3
"""Native/ARM checksum and bounded periodic signal delivery checks."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re

spec = importlib.util.spec_from_file_location("probes", Path(__file__).with_name("run_probes.py"))
probes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probes)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--runtime", required=True)
    p.add_argument("--output", required=True)
    p.add_argument("--signals", action="store_true")
    p.add_argument("--iterations", type=int, default=20000000)
    a = p.parse_args()
    output = Path(a.output)
    output.mkdir(parents=True, exist_ok=True)
    runtime = str(Path(a.runtime).resolve())
    result = dict(runtime=runtime, runtime_sha256=hashlib.sha256(Path(runtime).read_bytes()).hexdigest(),
                  iterations=a.iterations, builds={}, probes={})
    env = dict(os.environ, MACLATOR_JIT_THRESHOLD="1")
    for arch in ["x86_64", "arm64"]:
        binary = output / ("finite-loops-" + arch)
        command = ["/usr/bin/clang", "-arch", arch, "-O2", "-std=c11",
                   str(Path(__file__).with_name("loops_signal.c")), "-o", str(binary)]
        build = probes.run(command, output / (arch + "-build.txt"), 25, env)
        result["builds"][arch] = build
        if build["exit_code"]:
            continue
        for mode in (["plain", "signal"] if a.signals else ["plain"]):
            name = arch + "-" + mode
            command = [str(binary), mode, str(a.iterations)]
            if arch == "arm64":
                command = [runtime, "--no-aot"] + command
            check = probes.run(command, output / (name + ".txt"), 25, env)
            match = re.search(r"checksum=([0-9a-f]+) alarms=(\d+)", Path(check["log"]).read_text())
            if match:
                check.update(checksum=match[1], alarms=int(match[2]))
            result["probes"][name] = check
            print(name + ": exit=" + str(check["exit_code"]), flush=True)
    expected = result["probes"].get("x86_64-plain", {}).get("checksum")
    result["all_checks_pass"] = bool(expected) and len(result["probes"]) == (4 if a.signals else 2) and all(
        not row["exit_code"] and row.get("checksum") == expected and
        ("signal" not in name or row.get("alarms", 0) > 0) for name, row in result["probes"].items())
    (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    print("all_checks_pass=" + str(result["all_checks_pass"]), flush=True)


if __name__ == "__main__":
    main()
