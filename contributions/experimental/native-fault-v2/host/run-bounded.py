#!/usr/bin/env python3
"""Later runtime supervisor. Each case is a new process, killed at <=20s."""
import argparse
import datetime
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import source_guard

ROOT = Path(__file__).resolve().parent
PUBLIC_CASES = [
    "forward-source-start", "forward-source-mid", "forward-write-start", "forward-write-mid",
    "backward-source-start", "backward-source-mid", "backward-write-start", "backward-write-mid",
    "fill-start", "fill-mid", "success-decline", "output-alias", "default-flags",
    "custom-segv", "custom-bus", "simple-segv", "simple-bus", "ignore-segv", "ignore-bus",
    "oneshot-segv", "oneshot-bus", "nonleaf-default", "default-segv", "default-bus",
]
INTERNAL_CASES = ["target-context", "target-registry", "reentrant-preparing", "reentrant-armed"]
INSTALL_CASES = [
    "reject-custom-segv", "reject-custom-bus", "reject-ignore-segv", "reject-ignore-bus",
    "reject-oneshot-segv", "reject-oneshot-bus", "query-first-fails", "query-second-fails",
    "write-first-fails", "write-second-fails", "rollback-hidden-default", "rollback-fails", "default-success",
]
FATAL_SIGNALS = {
    "nonleaf-default": {int(signal.SIGSEGV), int(signal.SIGBUS)},
    "default-segv": {int(signal.SIGSEGV)}, "default-bus": {int(signal.SIGBUS)},
    "oneshot-segv": {int(signal.SIGSEGV)}, "oneshot-bus": {int(signal.SIGBUS)},
}

def run_case(binary, name, kind, timeout, output):
    started = time.monotonic()
    process = subprocess.Popen([str(binary), name], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                               text=True, start_new_session=True, close_fds=True)
    timed_out = False
    drain_error = None
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            stdout, stderr = process.communicate(timeout=2)
        except subprocess.TimeoutExpired:
            stdout, stderr = "", "supervisor could not drain output after process-group kill"
            drain_error = "pipe drain timeout"
            process.kill()
            process.wait(timeout=2)
    elapsed = time.monotonic() - started
    (output / (name + ".stdout")).write_text(stdout)
    (output / (name + ".stderr")).write_text(stderr)
    observations = []
    for line in stdout.splitlines():
        try:
            item = json.loads(line)
            if isinstance(item, dict):
                observations.append(item)
        except json.JSONDecodeError:
            pass
    expected_signals = FATAL_SIGNALS.get(name)
    if expected_signals:
        trigger_ready = any(item.get("fatal_trigger_ready") is True for item in observations)
        passed = (not timed_out and process.returncode is not None
                  and process.returncode < 0 and -process.returncode in expected_signals
                  and trigger_ready and "FAIL:" not in stderr)
        if name.startswith("oneshot-"):
            passed = passed and any(item.get("oneshot_first_callback") is True and item.get("pass") is True
                                    for item in observations)
        termination = "signaled" if process.returncode is not None and process.returncode < 0 else "normal-exit"
        # Exit 128+signal is a failure: it is the fatal fallback, not proof of
        # signal termination/WIFSIGNALED.
    else:
        markers = [item["pass"] for item in observations if "pass" in item]
        passed = (not timed_out and process.returncode == 0 and bool(markers)
                  and all(value is True for value in markers) and "FAIL:" not in stderr)
        termination = "signaled" if process.returncode is not None and process.returncode < 0 else "normal-exit"
    record = {
        "case": name, "kind": kind, "pass": passed, "returncode": process.returncode,
        "termination": termination, "timed_out": timed_out, "timeout_seconds": timeout,
        "elapsed_seconds": round(elapsed, 6), "observations": observations,
        "expected_signals": sorted(expected_signals) if expected_signals else [],
        "stdout": str(output / (name + ".stdout")), "stderr": str(output / (name + ".stderr")),
    }
    if drain_error:
        record["supervisor_error"] = drain_error
    print(("PASS " if passed else "FAIL ") + kind + " " + name, flush=True)
    return record

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--suite", choices=("public", "internal", "install", "all"), default="public")
    parser.add_argument("--timeout", type=float, default=20)
    parser.add_argument("--build", type=Path, default=ROOT / "build")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--case", action="append", help="optional exact case selection within chosen suite")
    args = parser.parse_args()
    if not 0 < args.timeout <= 20:
        parser.error("--timeout must be greater than zero and at most 20 seconds")
    source_identity = source_guard.verify()
    build_identity = json.loads((args.build / "build-info.json").read_text())
    for key in ("native_source_manifest_sha256", "probe_source_manifest_sha256"):
        if build_identity.get(key) != source_identity[key]:
            raise RuntimeError("built source identity does not match v2 source: " + key)
    available = {item["name"]: item for item in build_identity["binaries"]}
    selected = []
    if args.suite in ("public", "all"):
        selected.extend(("host-fault-probe", name, "public-real") for name in PUBLIC_CASES)
    if args.suite in ("internal", "all"):
        selected.extend(("internal-rejection-probe", name, "whitebox-state") for name in INTERNAL_CASES)
    if args.suite in ("install", "all"):
        selected.extend(("install-fault-probe", name, "whitebox-installer-real-actions") for name in INSTALL_CASES)
    if args.case:
        unknown = set(args.case) - {name for _, name, _ in selected}
        if unknown:
            parser.error("cases outside selected suite: " + ", ".join(sorted(unknown)))
        selected = [entry for entry in selected if entry[1] in set(args.case)]
    for binary_name in {name for name, _, _ in selected}:
        if binary_name not in available:
            raise RuntimeError("binary was not built in this build identity: " + binary_name)
        binary = args.build / binary_name
        if source_guard.sha256(binary) != available[binary_name]["sha256"]:
            raise RuntimeError("binary hash differs from build identity: " + binary_name)
    timestamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    output = args.output or ROOT / ("results-" + timestamp)
    output.mkdir(parents=True, exist_ok=False)
    records = []
    try:
        for binary_name, name, kind in selected:
            records.append(run_case(args.build / binary_name, name, kind, args.timeout, output))
            (output / "results.json").write_text(json.dumps(records, indent=2) + "\n")
    finally:
        summary = {
            "scope": "v2 host-only prototype; public hardware, internal state, and installer injection remain separate",
            "source_identity": source_identity, "build_identity": build_identity,
            "selected_cases": len(selected), "completed_cases": len(records),
            "passed_cases": sum(item["pass"] for item in records),
            "failed_cases": [item["case"] for item in records if not item["pass"]],
            "guest_resume_or_signal_delivery_implemented": False,
            "performance_measured": False,
            "hidden_raw_RESET_restoration_claimed": False,
            "historical_logs_not_in_public_package": True,
        }
        (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print("Results: " + str(output), flush=True)
    return 0 if len(records) == len(selected) and all(item["pass"] for item in records) else 1

if __name__ == "__main__":
    sys.exit(main())
