#!/usr/bin/env python3
"""Compare real native and translated Metal rendering, with bounded process groups."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--sysroot", type=Path, required=True)
    parser.add_argument("--gpu-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--baseline-gpu-dir", type=Path)
    parser.add_argument("--clang", default=shutil.which("clang"))
    parser.add_argument("--deadline", type=float, default=30)
    args = parser.parse_args()
    if not args.clang or not 0 < args.deadline <= 60:
        parser.error("clang is required; deadline must be between 0 and 60 seconds")
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    source = Path(__file__).resolve().parent / "gpu/metal_render_reflection.m"
    records = []

    def run(name, command, extra_env=None, expected_pass=True):
        env = dict(os.environ)
        for key in ("MCL_NOFAST", "MCL_NOBATCH", "MCL_NOCACHE", "MCL_OLDDESC", "MCL_NIL", "MCL_TRACE",
                    "MCL_STATS", "MACLATOR_GPU_BRIDGE", "DYLD_INSERT_LIBRARIES"):
            env.pop(key, None)
        env.update(extra_env or {})
        start = time.monotonic()
        process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True, start_new_session=True)
        timed_out = False
        try:
            stdout, stderr = process.communicate(timeout=args.deadline)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGKILL)
            stdout, stderr = process.communicate()
        except BaseException:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            raise
        (args.output / f"{name}.stdout.log").write_text(stdout)
        (args.output / f"{name}.stderr.log").write_text(stderr)
        record = {"name": name, "command": command, "environment": extra_env or {},
                  "returncode": process.returncode, "timeout": timed_out,
                  "elapsed_seconds": time.monotonic() - start}
        if expected_pass:
            record["passed"] = not timed_out and process.returncode == 0
        else:
            # Confirm this precise regression, rather than counting any unrelated failure.
            record["passed"] = not timed_out and process.returncode != 0 and "Invalid domain=nil" in stderr
        if name.startswith("native") or name.startswith("arm-"):
            required = ["PASS: render reflection request 1", "PASS: render reflection request 2",
                        "PASS: hardware vertex + fragment draw", "PASS: hardware compute reflection",
                        "PASS: separate NSError output"]
            record["passed"] &= all(marker in stdout for marker in required)
        records.append(record)
        (args.output / "results.json").write_text(json.dumps(records, indent=2) + "\n")
        print(f"{name}: {'PASS' if record['passed'] else 'FAIL'} ({record['elapsed_seconds']:.2f}s)", flush=True)
        return record["passed"]

    for arch in ("x86_64", "arm64"):
        binary = args.output / f"probe-{arch}"
        if not run(f"compile-{arch}", [args.clang, "-arch", arch, "-fobjc-arc", "-O1",
                                     "-Wno-deprecated-declarations", "-framework", "Foundation",
                                     "-framework", "Metal", str(source), "-o", str(binary)]):
            return 1
    results = [run("native-x86_64", ["/usr/bin/nice", "-n", "10", str(args.output / "probe-x86_64")])]
    command = ["/usr/bin/nice", "-n", "10", str(args.runtime.resolve()), "--no-aot", "--gpu",
               "--sysroot", str(args.sysroot.resolve()), str(args.output / "probe-arm64")]
    if args.baseline_gpu_dir:
        results.append(run("baseline-regression", command,
                           {"MACLATOR_GPU_DIR": str(args.baseline_gpu_dir.resolve())}, expected_pass=False))
    for name, extra in (("arm-default", {}), ("arm-legacy", {"MCL_NOFAST": "1", "MCL_OLDDESC": "1"}),
                        ("arm-unbatched", {"MCL_NOBATCH": "1"})):
        results.append(run(name, command, {"MACLATOR_GPU_DIR": str(args.gpu_dir.resolve()), **extra}))
    paths = [source, args.runtime.resolve(), args.gpu_dir / "libmclbridge.dylib",
             args.gpu_dir / "libmclmetal.dylib", args.output / "probe-x86_64", args.output / "probe-arm64"]
    sha = {str(path.resolve()): hashlib.sha256(path.read_bytes()).hexdigest() for path in paths}
    (args.output / "sha256.json").write_text(json.dumps(sha, indent=2) + "\n")
    return 0 if all(results) else 1


if __name__ == "__main__":
    raise SystemExit(main())
