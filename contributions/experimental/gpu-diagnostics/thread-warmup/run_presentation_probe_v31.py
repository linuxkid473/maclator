#!/usr/bin/env python3
"""Parent-run v3.1 diagnostic: v3 pthread path with 0.2s post-install warmup, twelve unchanged frames without explicit main-loop pumping, no installation."""
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


def warmup_precedes_critical(stdout, critical_begin):
    """Regex spans are half-open; adjacent complete log lines are valid."""
    warmup_begin = list(re.finditer(r"POST_INSTALL_WARMUP_BEGIN: seconds=0.2 main_thread=1\n", stdout))
    warmup_end = list(re.finditer(r"POST_INSTALL_WARMUP_END: main_thread=1\n", stdout))
    return (len(warmup_begin) == len(warmup_end) == 1 and bool(critical_begin) and
            warmup_begin[0].end() <= warmup_end[0].start() and
            warmup_end[0].end() <= critical_begin.start())


def warmup_parser_selfcheck():
    # Source-data-only oracle checks: never build, execute a probe, or alter logs.
    start = "POST_INSTALL_WARMUP_BEGIN: seconds=0.2 main_thread=1\n"
    end = "POST_INSTALL_WARMUP_END: main_thread=1\n"
    critical = "NO_MAIN_PUMP_BEGIN: requested_frames=12 pool_count=3 main_thread=1\n"
    cases = (
        ("adjacent_lines", start + end + critical, True),
        ("intervening_lines", start + "STAGE: warmup\n" + end + "STAGE: ready\n" + critical, True),
        ("end_before_begin", end + start + critical, False),
        ("critical_before_end", start + critical + end, False),
        ("missing_begin", end + critical, False),
        ("missing_end", start + critical, False),
        ("missing_critical", start + end, False),
        ("duplicate_begin", start + start + end + critical, False),
        ("duplicate_end", start + end + end + critical, False),
    )
    checks = []
    for name, stdout, expected in cases:
        begin = re.search(r"NO_MAIN_PUMP_BEGIN: requested_frames=12 pool_count=3 main_thread=1", stdout)
        actual = bool(warmup_precedes_critical(stdout, begin))
        checks.append({"case": name, "expected": expected, "actual": actual, "passed": actual == expected})
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runtime", type=Path, required=True)
    parser.add_argument("--sysroot", type=Path, required=True)
    parser.add_argument("--gpu-dir", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--modes", choices=("cametal-thread", "mtkview-thread", "all"), required=True)
    parser.add_argument("--arm-mode", choices=("default", "legacy", "unbatched"), default="default")
    parser.add_argument("--deadline", type=float, default=30)
    parser.add_argument("--clang", default="/usr/bin/clang")
    args = parser.parse_args()
    parser_checks = warmup_parser_selfcheck()
    if not all(check["passed"] for check in parser_checks):
        parser.error("Warmup parser data-only self-check failed")
    if not 0 < args.deadline <= 30:
        parser.error("Each process deadline must be >0 and <=30 seconds")
    source = Path(__file__).with_name("presentation_probe_v31.m").resolve()
    inputs = [source, Path(__file__).resolve(), args.runtime.resolve(),
              args.gpu_dir.resolve() / "libmclmetal.dylib",
              args.gpu_dir.resolve() / "libmclbridge.dylib"]
    before = {str(p): sha(p) for p in inputs}
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    if (args.output / "probe-x86_64").exists() or (args.output / "probe-arm64").exists():
        parser.error("Use a fresh output directory; refusing to overwrite an existing probe binary")
    (args.output / "inputs-before.json").write_text(json.dumps(before, indent=2) + "\n")
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
                          r"presented_callbacks=(\d+) worker_returns=(\d+) callbacks_before_drain=(\d+) "
                          r"positive_times=(\d+) zero_times=(\d+) callback_metadata_errors=(\d+) "
                          r"explicit_pump_calls=(\d+) screen_verified=false", stdout)
        result = None
        if mode:
            valid = valid and bool(match)
            if match:
                keys = ("completed_frames", "verified_surface_pixels", "presented_callbacks", "worker_returns",
                        "callbacks_before_drain", "positive_presented_times", "zero_presented_times", "callback_metadata_errors", "explicit_pump_calls")
                result = {"api_gpu_checks": match.group(1), **dict(zip(keys, map(int, match.groups()[1:]))), "screen_verified": False}
                metadata = re.findall(r"DRAWABLE_META: frame=(\d+) main_thread=(\d+) drawable=(\S+) texture=(\S+) layer=(\S+) layer_device=(\S+) texture_device=(\S+) texture_storage=(\d+) framebuffer_only=(\d+)", stdout)
                result["drawable_metadata"] = [dict(zip(("frame", "main_thread", "drawable", "texture", "layer", "layer_device", "texture_device", "texture_storage", "framebuffer_only"), row)) for row in metadata]
                worker_begins = list(re.finditer(r"WORKER_PRESENT_BEGIN: frame=(\d+) main_thread=(\d+) pthread=(\S+) drawable=(\S+)", stdout))
                worker_returns = list(re.finditer(r"WORKER_PRESENT_RETURNED: frame=(\d+) main_thread=0", stdout))
                gpu_complete = list(re.finditer(r"GPU_COMPLETE: frame=(\d+)\n", stdout))
                callbacks = list(re.finditer(r"REAL_PRESENTED_CALLBACK: count=(\d+) frame=(\d+) drawable=(\S+) main_thread=(\d+) presented_time=([\d.]+)", stdout))
                begin = re.search(r"NO_MAIN_PUMP_BEGIN: requested_frames=12 pool_count=3 main_thread=1", stdout)
                end = re.search(r"NO_MAIN_PUMP_END: completed_frames=(\d+) worker_present_returns=(\d+) callbacks_during_phase=(\d+) explicit_pump_calls=(\d+)", stdout)
                expected = list(range(12))
                sequence = ([int(m.group(1)) for m in gpu_complete] == expected and
                            [int(m.group(1)) for m in worker_begins] == expected and
                            [int(m.group(1)) for m in worker_returns] == expected and
                            all(m.group(2) == "0" for m in worker_begins) and
                            bool(begin) and bool(end))
                if sequence:
                    sequence = (all(begin.end() < g.start() < b.start() < w.start() < end.start()
                                    for g, b, w in zip(gpu_complete, worker_begins, worker_returns)) and
                                int(end.group(1)) == 12 and int(end.group(2)) == 12 and
                                int(end.group(3)) == result["callbacks_before_drain"] and int(end.group(4)) == 0)
                result["gpu_fence_before_worker_present_sequence"] = bool(sequence)
                result["real_callback_frame_ids"] = sorted(int(m.group(2)) for m in callbacks)
                result["real_callback_count_ids"] = sorted(int(m.group(1)) for m in callbacks)
                result["critical_phase_ended"] = bool(end)
                warmup_sequence = warmup_precedes_critical(stdout, begin)
                result["post_install_warmup_before_critical"] = bool(warmup_sequence)
                # Recompute finite real times from raw callbacks, rather than
                # inferring visibility from the count or trusting counters alone.
                raw_times = [float(m.group(5)) for m in callbacks]
                raw_positive = sum(value > 0 for value in raw_times)
                raw_zero = sum(value == 0 for value in raw_times)
                result["raw_positive_presented_times"] = raw_positive
                result["raw_zero_presented_times"] = raw_zero
                valid = valid and result["api_gpu_checks"] == "PASS"
                valid = valid and result["completed_frames"] == result["worker_returns"] == result["presented_callbacks"] == 12
                valid = valid and result["callback_metadata_errors"] == result["explicit_pump_calls"] == 0
                valid = valid and result["positive_presented_times"] + result["zero_presented_times"] == 12
                valid = valid and [int(row[0]) for row in metadata] == expected and all(row[1] == "1" for row in metadata)
                valid = valid and result["real_callback_frame_ids"] == expected and result["real_callback_count_ids"] == list(range(1, 13))
                valid = valid and sequence and warmup_sequence
                valid = valid and raw_positive == result["positive_presented_times"] and raw_zero == result["zero_presented_times"]
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
    modes = ("cametal-thread", "mtkview-thread") if args.modes == "all" else (args.modes,)
    results = []
    for mode in modes:
        results.append(run(f"native-{mode}", [str(args.output / "probe-x86_64"), mode], mode=mode))
        extra = {"MACLATOR_GPU_DIR": str(args.gpu_dir.resolve())}
        if args.arm_mode == "legacy":
            extra.update({"MCL_NOFAST": "1", "MCL_OLDDESC": "1"})
        elif args.arm_mode == "unbatched":
            extra["MCL_NOBATCH"] = "1"
        results.append(run(f"arm-{args.arm_mode}-{mode}",
                           [str(args.runtime.resolve()), "--no-aot", "--gpu", "--sysroot",
                            str(args.sysroot.resolve()), str(args.output / "probe-arm64"), mode],
                           extra, mode))
    after = {str(p): sha(p) for p in inputs}
    identity = {"before": before, "after": after, "unchanged": before == after,
                "binaries": {str(args.output / f"probe-{arch}"): sha(args.output / f"probe-{arch}")
                             for arch in ("x86_64", "arm64")},
                "claim": "v3.1 adds only a bounded 0.2s main-loop warmup after new layer/view installation to the original v3 worker-thread path; 12 finite frames with no explicit main run-loop pump during acquisition loop. Callback metadata drained afterward; no visual, general thread-safety, long-run or performance claim."}
    (args.output / "sha256.json").write_text(json.dumps(identity, indent=2) + "\n")
    return 0 if all(results) and identity["unchanged"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
