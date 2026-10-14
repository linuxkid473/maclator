#!/usr/bin/env python3
"""Parent-only Rust adapter supervisor. Fresh process per mode/case, <=20s."""
import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import time
import source_guard
ROOT = Path(__file__).resolve().parent
CASES = ["layout", "startup-prior", "install-custom-rejected", "install-report", "complete-copy", "complete-fill",
         "decline-invalid", "decline-blocked", "reentrant-preparing", "reentrant-armed",
         "fault-copy-forward", "fault-copy-backward", "fault-fill"]
BINARIES = {"direct": "adapter-direct-probe", "observed": "adapter-observed-probe"}

def stop_and_drain(process):
    errors = []
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    except OSError as error:
        errors.append("process-group kill: " + type(error).__name__)
        try:
            process.kill()
        except ProcessLookupError:
            pass
        except OSError as kill_error:
            errors.append("direct kill: " + type(kill_error).__name__)
    try:
        stdout, stderr = process.communicate(timeout=2)
    except subprocess.TimeoutExpired as error:
        errors.append("pipe drain timeout")
        stdout = error.stdout or ""
        stderr = error.stderr or ""
        if isinstance(stdout, bytes):
            stdout = stdout.decode(errors="replace")
        if isinstance(stderr, bytes):
            stderr = stderr.decode(errors="replace")
        for pipe in (process.stdout, process.stderr):
            if pipe is not None:
                pipe.close()
        try:
            process.kill()
        except ProcessLookupError:
            pass
        except OSError as kill_error:
            errors.append("drain direct kill: " + type(kill_error).__name__)
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            errors.append("reap timeout")
    return stdout, stderr, errors

def run_case(binary, name, mode, timeout, output):
    key = mode + "-" + name
    started = time.monotonic()
    process = subprocess.Popen([str(binary), name], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True, close_fds=True)
    timed_out = False
    cleanup_errors = []
    stdout, stderr = "", ""
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        stdout, stderr, cleanup_errors = stop_and_drain(process)
    except BaseException as error:
        try:
            stdout, stderr, cleanup_errors = stop_and_drain(process)
        finally:
            (output / (key + ".stdout")).write_text(stdout)
            (output / (key + ".stderr")).write_text(stderr)
            abort = {"case": name, "mode": mode, "key": key, "pass": False,
                     "exception_type": type(error).__name__, "returncode": process.returncode,
                     "cleanup_errors": cleanup_errors,
                     "elapsed_seconds": round(time.monotonic()-started, 6)}
            (output / (key + ".supervisor-abort.json")).write_text(json.dumps(abort, indent=2)+"\n")
        raise
    (output / (key + ".stdout")).write_text(stdout)
    (output / (key + ".stderr")).write_text(stderr)
    observations = []
    for line in stdout.splitlines():
        try:
            item = json.loads(line)
            if isinstance(item, dict):
                observations.append(item)
        except json.JSONDecodeError:
            pass
    final = [item for item in observations if item.get("case") == name and item.get("mode") == mode and "pass" in item]
    passed = (not timed_out and not cleanup_errors and process.returncode == 0 and bool(final)
              and all(item["pass"] is True for item in final))
    record = {"case": name, "mode": mode, "key": key, "pass": passed, "returncode": process.returncode,
              "timed_out": timed_out, "timeout_seconds": timeout,
              "elapsed_seconds": round(time.monotonic()-started, 6), "observations": observations,
              "stdout": str(output / (key + ".stdout")), "stderr": str(output / (key + ".stderr")),
              "kind": "rust-direct-ffi-whitebox-state" if mode == "direct" else "rust-observed-whitebox-call-boundary"}
    if cleanup_errors:
        record["supervisor_errors"] = cleanup_errors
    print(("PASS " if passed else "FAIL ") + key, flush=True)
    return record

def evidence_snapshot(build, modes):
    identity = source_guard.verify()
    build_info_path = build / "build-info.json"
    build_info = json.loads(build_info_path.read_text())
    for key in ("native_source_manifest_sha256", "probe_source_manifest_sha256", "public_package_manifest_sha256"):
        if identity[key] != build_info.get(key):
            raise RuntimeError("built/source identity mismatch: " + key)
    known = {entry["name"]: entry for entry in build_info["binaries"]}
    binary_hashes = {}
    for mode in modes:
        name = BINARIES[mode]
        digest = source_guard.sha256(build/name)
        if name not in known or digest != known[name]["sha256"]:
            raise RuntimeError("binary identity mismatch: " + name)
        binary_hashes[name] = digest
    compiler_hashes = {}
    for entry in build_info["compiler_files"]:
        digest = source_guard.sha256(build/entry["name"])
        if digest != entry["sha256"]:
            raise RuntimeError("compiler metadata changed")
        compiler_hashes[entry["name"]] = digest
    return {"source_identity": identity, "build_info_sha256": source_guard.sha256(build_info_path),
            "binaries": binary_hashes, "compiler_files": compiler_hashes}, build_info

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", choices=("direct", "observed", "all"), default="all")
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--build", type=Path, default=ROOT / "build")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--case", action="append")
    args = parser.parse_args()
    if not 0 < args.timeout <= 20:
        parser.error("--timeout must be >0 and <=20 seconds")
    if args.case and set(args.case)-set(CASES):
        parser.error("unknown case: " + ", ".join(sorted(set(args.case)-set(CASES))))
    modes = list(BINARIES) if args.suite == "all" else [args.suite]
    cases = [name for name in CASES if not args.case or name in args.case]
    before, build_info = evidence_snapshot(args.build, modes)
    identity = before["source_identity"]
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = args.output or ROOT / ("results-" + timestamp)
    output.mkdir(parents=True, exist_ok=False)
    records = []
    evidence_unchanged = False
    try:
        for mode in modes:
            for name in cases:
                records.append(run_case(args.build/BINARIES[mode], name, mode, args.timeout, output))
                (output/"results.json").write_text(json.dumps(records, indent=2)+"\n")
    finally:
        try:
            after, _ = evidence_snapshot(args.build, modes)
            evidence_unchanged = after == before
            identity_check = {"before": before, "after": after, "unchanged": evidence_unchanged,
                              "scope": "start/end snapshots only; no claim of continuous immutability"}
        except Exception as error:
            identity_check = {"before": before, "unchanged": False,
                              "end_verification_error": type(error).__name__ + ": " + str(error),
                              "scope": "start/end snapshots only; no claim of continuous immutability"}
        summary = {"scope": "standalone frozen Rust adapter ABI and normal-return ownership only",
                   "source_identity": identity, "build_identity": build_info,
                   "evidence_identity_check": identity_check,
                   "selected_cases": len(modes)*len(cases), "completed_cases": len(records),
                   "passed_cases": sum(item["pass"] for item in records),
                   "failed_cases": [item["key"] for item in records if not item["pass"]],
                   "direct_and_observed_evidence_separate": True,
                   "production_prior_reset_authorized": False,
                   "shared_runtime_registered": False, "guest_resume_implemented": False,
                   "performance_measured": False, "95_percent_target_achieved": False}
        (output/"summary.json").write_text(json.dumps(summary, indent=2)+"\n")
        print("Results: " + str(output), flush=True)
    return 0 if evidence_unchanged and len(records) == len(modes)*len(cases) and all(item["pass"] for item in records) else 1

if __name__ == "__main__":
    raise SystemExit(main())
