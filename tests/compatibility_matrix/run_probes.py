#!/usr/bin/env python3
"""Build our bounded synthetic probes; preserve failures instead of claiming coverage."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def run(command, log, timeout, env):
    start = time.monotonic()
    with log.open("w") as out:
        child = subprocess.Popen(["/usr/bin/nice", "-n", "10"] + command, stdout=out,
                                 stderr=subprocess.STDOUT, start_new_session=True, env=env)
        try:
            status = child.wait(timeout=timeout)
            timed_out = False
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait()
            status, timed_out = child.returncode, True
    return dict(command=command, exit_code=status, timeout=timed_out,
                seconds=round(time.monotonic() - start, 3), log=str(log))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--runtime", required=True)
    p.add_argument("--output", required=True)
    p.add_argument("--jit-threshold", type=int, default=24)
    a = p.parse_args()
    base = Path(__file__).resolve().parent
    output = Path(a.output)
    output.mkdir(parents=True, exist_ok=True)
    runtime = str(Path(a.runtime).resolve())
    result = dict(runtime=runtime, runtime_sha256=hashlib.sha256(Path(runtime).read_bytes()).hexdigest(),
                  scope="bounded synthetic C++ EH/TLS, Foundation/dispatch, ARM JIT rewrite and atomics probes",
                  builds={}, probes={})
    env = dict(os.environ, MACLATOR_JIT_THRESHOLD=str(a.jit_threshold))
    cases = [
        ("exceptions", "/usr/bin/clang++", ["-std=c++17", "-pthread"], "exceptions.cpp"),
        ("foundation", "/usr/bin/clang", ["-fblocks", "-framework", "Foundation"], "foundation.m"),
        ("jit_rewrite", "/usr/bin/clang", ["-std=c11", "-pthread"], "jit_rewrite.c"),
        ("atomics-apple-m1", "/usr/bin/clang", ["-std=c11", "-pthread"], "atomics.c"),
        ("atomics-llsc", "/usr/bin/clang", ["-std=c11", "-pthread", "-mcpu=generic", "-march=armv8-a"], "atomics.c"),
        ("atomics-lse", "/usr/bin/clang", ["-std=c11", "-pthread", "-mcpu=generic", "-march=armv8.1-a"], "atomics.c"),
        ("pair-access", "/usr/bin/clang", ["-std=c11"], "pair_access.c"),
    ]
    for name, compiler, flags, source in cases:
        binary = output / name
        command = [compiler, "-arch", "arm64", "-O2"] + flags + [str(base / source), "-o", str(binary)]
        build = run(command, output / (name + "-build.txt"), 25, env)
        result["builds"][name] = build
        if build["exit_code"] == 0:
            checks = [(name, [])] if name != "jit_rewrite" else [("jit-sequential", []), ("jit-cross-thread", ["thread"])]
            if name.startswith("atomics-"):
                checks.append((name + "-store", ["20000", "store"]))
            for check, args in checks:
                probe = run([runtime, "--no-aot", str(binary)] + args,
                            output / (check + ".txt"), 25, env)
                probe["result"] = "TIMEOUT" if probe["timeout"] else "PASS" if probe["exit_code"] == 0 else "FAIL"
                result["probes"][check] = probe
                print(check + ": " + probe["result"], flush=True)
        else:
            print(name + ": BUILD_FAILED", flush=True)
        (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
