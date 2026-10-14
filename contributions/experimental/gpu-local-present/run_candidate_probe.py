#!/usr/bin/env python3
"""Parent-run candidate diagnostics: original presentDrawable route in three bridge modes, no installation."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--sysroot", type=Path, required=True)
    parser.add_argument("--gpu-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--modes", choices=("cametal", "mtkview", "all"), required=True)
    parser.add_argument("--storage", choices=("managed", "shared"), default="managed")
    parser.add_argument("--arm-mode", choices=("default", "legacy", "unbatched", "all"), default="all")
    parser.add_argument("--deadline", type=float, default=30)
    parser.add_argument("--clang", default="/usr/bin/clang")
    args = parser.parse_args()
    if not 0 < args.deadline <= 30:
        parser.error("Each process deadline must be >0 and <=30 seconds")
    source = Path(__file__).with_name("presentation_probe.m").resolve()
    inputs = [source, Path(__file__).resolve(), args.runtime.resolve(),
              args.gpu_dir.resolve() / "libmclmetal.dylib",
              args.gpu_dir.resolve() / "libmclbridge.dylib"]
    before = {str(p): sha(p) for p in inputs}
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    records = []

    def kill_owned_group(process):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass  # probe exited between its deadline and cleanup

    def run(name, command, extra_env=None, mode=None):
        env = dict(os.environ)
        for key in list(env):
            if key.startswith(("MCL_", "MACLATOR_", "DYLD_")):
                env.pop(key, None)
        env.update(extra_env or {})
        start = time.monotonic()
        process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, start_new_session=True)
        timed_out = False
        try:
            stdout, stderr = process.communicate(timeout=args.deadline)
        except subprocess.TimeoutExpired:
            timed_out = True
            kill_owned_group(process)  # only this probe's own group
            stdout, stderr = process.communicate()
        except BaseException:
            if process.poll() is None:
                kill_owned_group(process)
            process.communicate()
            raise
        (args.output / f"{name}.stdout.log").write_text(stdout)
        (args.output / f"{name}.stderr.log").write_text(stderr)
        valid = not timed_out and process.returncode == 0
        match = re.search(r"RESULT: api_gpu_checks=(PASS|FAIL) frames=(\d+) surface_pixels=(\d+) "
                          r"presented_callbacks=(\d+) screen_verified=false", stdout)
        result = None
        if mode:
            valid = valid and bool(match) and match.group(1) == "PASS" and int(match.group(2)) == 3
            if match:
                result = {"api_gpu_checks": match.group(1), "completed_frames": int(match.group(2)),
                          "verified_surface_pixels": int(match.group(3)),
                          "presented_callbacks": int(match.group(4)), "screen_verified": False}
                metadata = re.findall(r"DRAWABLE_META: frame=(\d+) main_thread=(\d+) drawable=(\S+) texture=(\S+) layer=(\S+) layer_device=(\S+) texture_device=(\S+)", stdout)
                present_returns = re.findall(r"LOCAL_PRESENT_RETURNED: frame=(\d+) main_thread=1", stdout)
                result["drawable_metadata"] = [dict(zip(("frame", "main_thread", "drawable", "texture", "layer", "layer_device", "texture_device"), row)) for row in metadata]
                result["local_present_return_frames"] = [int(v) for v in present_returns]
                result["real_callback_records"] = len(re.findall(r"REAL_PRESENTED_CALLBACK:", stdout))
                valid = valid and result["presented_callbacks"] == 3 and result["real_callback_records"] == 3
                valid = valid and [int(row[0]) for row in metadata] == [0, 1, 2]
                valid = valid and all(row[1] == "1" for row in metadata)
                if name.startswith("arm-"):
                    result["bridge_local_present_recorded"] = stderr.count("[mclmetal] local present recorded h=")
                    result["bridge_completion_armed"] = stderr.count("[mclmetal] local present completion armed h=")
                    result["bridge_guest_main_present_calls"] = stderr.count("[mclmetal] local present on guest main thread drawable=")
                    result["bridge_host_commit_accepted"] = stderr.count("[mclmetal] local present host commit accepted h=")
                    valid = valid and all(result[k] == 3 for k in ("bridge_local_present_recorded", "bridge_completion_armed", "bridge_guest_main_present_calls", "bridge_host_commit_accepted"))
        record = {"name": name, "pid": process.pid, "command": command,
                  "extra_environment": extra_env or {}, "returncode": process.returncode,
                  "timeout": timed_out, "elapsed_seconds": time.monotonic() - start,
                  "api_gpu_checks_passed": valid, "result": result,
                  "screen_confirmation": "not performed by this runner"}
        records.append(record)
        (args.output / "results.json").write_text(json.dumps(records, indent=2) + "\n")
        print(f"{name}: API/GPU {'PASS' if valid else 'FAIL'}; screen unverified", flush=True)
        return valid

    for arch in ("x86_64", "arm64"):
        binary = args.output / f"probe-{arch}"
        if not run(f"compile-{arch}", [args.clang, "-arch", arch, "-fobjc-arc", "-O1",
                                      "-Wno-deprecated-declarations", "-framework", "AppKit",
                                      "-framework", "Metal", "-framework", "MetalKit",
                                      "-framework", "QuartzCore", "-framework", "IOSurface",
                                      str(source), "-o", str(binary)]):
            return 1
    modes = ("cametal", "mtkview") if args.modes == "all" else (args.modes,)
    arm_modes = ("default", "legacy", "unbatched") if args.arm_mode == "all" else (args.arm_mode,)
    results = []
    for mode in modes:
        results.append(run(f"native-{mode}", [str(args.output / "probe-x86_64"), mode, args.storage], mode=mode))
        for arm_mode in arm_modes:
            extra = {"MACLATOR_GPU_DIR": str(args.gpu_dir.resolve()), "MCL_LOCAL_PRESENT": "1", "MCL_TRACE": "1"}
            if arm_mode == "legacy":
                extra.update({"MCL_NOFAST": "1", "MCL_OLDDESC": "1"})
            elif arm_mode == "unbatched":
                extra["MCL_NOBATCH"] = "1"
            results.append(run(f"arm-{arm_mode}-{mode}",
                               [str(args.runtime.resolve()), "--no-aot", "--gpu", "--sysroot",
                                str(args.sysroot.resolve()), str(args.output / "probe-arm64"), mode, args.storage],
                               extra, mode))
    after = {str(p): sha(p) for p in inputs}
    identity = {"before": before, "after": after, "unchanged": before == after,
                "binaries": {str(args.output / f"probe-{arch}"): sha(args.output / f"probe-{arch}")
                             for arch in ("x86_64", "arm64")},
                "claim": "Original commandBuffer presentDrawable route with opt-in compatibility bridge; real callbacks required, screen and performance unverified"}
    (args.output / "sha256.json").write_text(json.dumps(identity, indent=2) + "\n")
    return 0 if all(results) and identity["unchanged"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
