#!/usr/bin/env python3
"""Nine interleaved observation/control pairs on a disposable CPU-only fixture.

This validation utility does not install services, acquire root, or change drivers.
"""
from __future__ import annotations

import argparse
import json
import os
import plistlib
from pathlib import Path
import statistics
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
CLI = Path("/Applications/MacFlow.app/Contents/MacOS/macflow")
FIXTURE = ROOT / "target/release/examples/compute_fixture"
PROBE = ROOT / "target/release/examples/interaction_probe"
PROBE_APP = ROOT / "target/validation/MacFlowInteractionProbe.app"


def package_probe():
    import shutil
    contents = PROBE_APP / "Contents"
    binary = contents / "MacOS/InteractionProbe"
    binary.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(PROBE, binary)
    with (contents / "Info.plist").open("wb") as stream:
        plistlib.dump({"CFBundleIdentifier": "org.macflow.validation.interaction", "CFBundleExecutable": "InteractionProbe", "CFBundleName": "MacFlow Interaction Probe", "CFBundlePackageType": "APPL", "CFBundleVersion": "1", "NSHighResolutionCapable": True, "NSPrincipalClass": "NSApplication"}, stream)
    subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(PROBE_APP)], check=True, capture_output=True)


def invoke(*args: str) -> dict:
    result = subprocess.run([str(CLI), *args], capture_output=True, text=True, timeout=8)
    if result.returncode:
        raise RuntimeError(f"macflow {' '.join(args)}: {result.stderr.strip()}")
    return json.loads(result.stdout)


def write_json(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix(path.suffix + ".tmp")
    temp.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n")
    temp.replace(path)


def nice_of(pid: int) -> int | None:
    try:
        return os.getpriority(os.PRIO_PROCESS, pid)
    except ProcessLookupError:
        return None


def evidence(pid: int) -> dict:
    status = invoke("status", "--json")
    telemetry = status.get("telemetry", {})
    process = next((p for p in telemetry.get("processes", []) if p.get("identity", {}).get("pid") == pid), None)
    return {
        "at_monotonic_seconds": time.monotonic(),
        "nice_observed": nice_of(pid),
        "helper_connected": status.get("helper_connected"),
        "active_leases": status.get("active_leases"),
        "pressure": status.get("pressure"),
        "config": status.get("config"),
        "cpu_percent": telemetry.get("cpu_percent"),
        "memory_pressure": telemetry.get("memory_pressure"),
        "foreground_pid": telemetry.get("foreground_pid"),
        "fixture_sample": process,
        "telemetry_errors": telemetry.get("errors", []),
    }


def scenario(pair: int, observation: bool, duration: int, threads: int, output: Path) -> dict:
    label = f"pair-{pair:02d}-{'observation' if observation else 'control'}"
    invoke("observe", "on" if observation else "off")
    # Let any previous lease reconciliation settle before the next disposable process.
    time.sleep(2.3)
    fixture = subprocess.Popen([str(FIXTURE), "--duration", str(duration), "--threads", str(threads)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    probe_file = output / f"{label}-probe.json"
    probe_process = subprocess.Popen(["/usr/bin/open", "-n", "-W", str(PROBE_APP), "--args", "--warmup", "12", "--duration", "3", "--label", label, "--json", str(probe_file)], stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    report = {"pair": pair, "label": label, "observation": observation, "pid": fixture.pid, "initial_nice": nice_of(fixture.pid)}
    try:
        report["before"] = evidence(fixture.pid)
        time.sleep(12)
        if fixture.poll() is not None:
            raise RuntimeError("fixture ended before the twelve-second warmup completed")
        report["warmup"] = evidence(fixture.pid)
        _, probe_error = probe_process.communicate(timeout=10)
        if probe_process.returncode:
            raise RuntimeError(f"interaction probe failed: {probe_error.strip()}")
        report["probe"] = json.loads(probe_file.read_text())
        report["after_probe"] = evidence(fixture.pid)
        stdout, stderr = fixture.communicate(timeout=duration + 5)
        if fixture.returncode:
            raise RuntimeError(f"compute fixture failed: {stderr.strip()}")
        report["fixture"] = json.loads(stdout)
        if observation:
            report["fixture_was_controlled"] = False
            report["valid_controller_comparison"] = all(report[k]["nice_observed"] == report["initial_nice"] for k in ("warmup", "after_probe"))
        else:
            report["fixture_was_controlled"] = any(
                report[k]["nice_observed"] is not None
                and report[k]["nice_observed"] > report["initial_nice"]
                and report[k]["helper_connected"] is True
                and (report[k]["active_leases"] or 0) > 0
                and report[k]["pressure"] is True
                for k in ("warmup", "after_probe")
            )
            report["valid_controller_comparison"] = report["fixture_was_controlled"]
        probe = report["probe"]
        report["interaction_thresholds"] = {
            "p95_le_50_ms": probe.get("p95_ms") is not None and probe["p95_ms"] <= 50,
            "p99_le_100_ms": probe.get("p99_ms") is not None and probe["p99_ms"] <= 100,
            "callback_failures": probe.get("callback_failures"),
            "skipped_pending_intervals": probe.get("skipped_intervals_while_callback_pending"),
        }
    except BaseException as error:
        report["error"] = str(error)
        report["valid_controller_comparison"] = False
        raise
    finally:
        if probe_process.poll() is None:
            probe_process.terminate()
            try:
                probe_process.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                probe_process.kill()
                probe_process.communicate(timeout=3)
        if fixture.poll() is None:
            fixture.terminate()
            try:
                fixture.communicate(timeout=3)
            except subprocess.TimeoutExpired:
                fixture.kill()
                fixture.communicate(timeout=3)
        write_json(output / f"{label}.json", report)
    return report


def pair_result(index: int, rounds: list[dict]) -> dict:
    baseline = next(r for r in rounds if r["observation"])
    control = next(r for r in rounds if not r["observation"])
    baseline_p95 = baseline["probe"].get("p95_ms")
    control_p95 = control["probe"].get("p95_ms")
    improve = 1 - control_p95 / baseline_p95 if baseline_p95 and control_p95 is not None else None
    baseline_rate = baseline["fixture"]["units_per_second"]
    control_rate = control["fixture"]["units_per_second"]
    estimated_penalty = baseline_rate / control_rate - 1 if control_rate else None
    valid = all(r["valid_controller_comparison"] for r in rounds)
    return {
        "pair": index,
        "valid_controller_comparison": valid,
        "observation_p95_ms": baseline_p95,
        "control_p95_ms": control_p95,
        "observation_p99_ms": baseline["probe"].get("p99_ms"),
        "control_p99_ms": control["probe"].get("p99_ms"),
        "observation_skipped_intervals": baseline["probe"]["skipped_intervals_while_callback_pending"],
        "control_skipped_intervals": control["probe"]["skipped_intervals_while_callback_pending"],
        "p95_improvement_fraction": improve,
        "p95_improvement_at_least_20_percent": valid and improve is not None and improve >= 0.20,
        "observation_units_per_second": baseline_rate,
        "control_units_per_second": control_rate,
        "estimated_fixed_work_duration_penalty_fraction": estimated_penalty,
        "estimated_duration_penalty_le_15_percent": valid and estimated_penalty is not None and estimated_penalty <= 0.15,
        "penalty_method": "inferred inverse throughput for this timed CPU fixture; fixed-work application duration was not measured",
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "artifacts/responsiveness-comparison")
    parser.add_argument("--duration", type=int, default=18)
    parser.add_argument("--threads", type=int, default=30)
    parser.add_argument("--pairs", type=int, default=9)
    parser.add_argument("--build-only", action="store_true", help="build cached Rust examples and validate configuration; do not run or change MacFlow")
    args = parser.parse_args()
    if not 18 <= args.duration <= 30 or not 1 <= args.threads <= 30 or not 1 <= args.pairs <= 9:
        parser.error("duration must be 18..30, threads 1..30, pairs 1..9")
    subprocess.run(["cargo", "build", "--release", "-p", "macflow", "--example", "compute_fixture", "--example", "interaction_probe"], cwd=ROOT, check=True)
    package_probe()
    if args.build_only:
        print(json.dumps({"built": [str(FIXTURE), str(PROBE)], "scenarios_executed": False}))
        return 0
    if os.geteuid() == 0:
        raise RuntimeError("run comparison as the logged-in user, never root")
    if not CLI.is_file():
        raise RuntimeError("signed MacFlow must already be installed; this runner does not install it")
    initial_status = invoke("status", "--json")
    initial = initial_status["config"]
    if not initial_status.get("helper_connected"):
        raise RuntimeError("administrator helper is not connected; no controlled comparison can be measured")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    summary = {"schema": 1, "scope": "native probe queue latency during one bounded CPU-only fixture", "initial_config": initial,
               "parameters": {"pairs": args.pairs, "duration_seconds": args.duration, "threads": args.threads, "warmup_seconds": 12, "probe_seconds": 3},
               "rounds": [], "pairs": [], "restoration_errors": [], "global_fps": None,
               "physical_input_to_display_latency_ms": None, "uncontended_regression_le_3_percent": None,
               "translation_native_efficiency": None, "24_hour_stress_test": "not performed", "seven_day_daily_use_test": "not performed"}
    selector = f"path:{FIXTURE.resolve()}"
    exit_code = 0
    try:
        invoke("mode", "auto")
        invoke("resume")
        invoke("rule", "background", selector)
        for index in range(1, args.pairs + 1):
            # Reverse order each pair to reduce systematic warmup/temperature bias.
            order = (True, False) if index % 2 else (False, True)
            rounds = []
            for observation in order:
                report = scenario(index, observation, args.duration, args.threads, output)
                rounds.append(report)
                summary["rounds"].append(report)
                print(json.dumps({"completed": report["label"], "controlled": report["fixture_was_controlled"], "valid": report["valid_controller_comparison"], "p95_ms": report["probe"]["p95_ms"], "p99_ms": report["probe"]["p99_ms"]}), flush=True)
                write_json(output / "summary.json", summary)
            summary["pairs"].append(pair_result(index, rounds))
            write_json(output / "summary.json", summary)
        valid = [p for p in summary["pairs"] if p["valid_controller_comparison"]]
        summary["assessment"] = {
            "valid_pairs": len(valid), "required_pairs": 9,
            "all_nine_controller_comparisons_valid": len(valid) == 9,
            "all_pairs_p95_improvement_at_least_20_percent": len(valid) == 9 and all(p["p95_improvement_at_least_20_percent"] for p in valid),
            "all_control_rounds_meet_p95_p99_thresholds": all(r["interaction_thresholds"]["p95_le_50_ms"] and r["interaction_thresholds"]["p99_le_100_ms"] for r in summary["rounds"] if not r["observation"]),
            "median_estimated_duration_penalty_fraction": statistics.median(p["estimated_fixed_work_duration_penalty_fraction"] for p in valid) if valid else None,
            "worst_pair_estimated_duration_penalty_fraction": max((p["estimated_fixed_work_duration_penalty_fraction"] for p in valid), default=None),
            "note": "Per-pair outcomes remain authoritative; no global smoothness, application compatibility, or native-95% claim is established.",
        }
        if len(valid) != args.pairs:
            exit_code = 2
    except BaseException as error:
        summary["run_error"] = str(error)
        exit_code = 1
    finally:
        # Every cleanup is attempted independently, even if a service stopped.
        for command in [("rule", "protect", selector), ("observe", "on" if initial["observe_only"] else "off"),
                        ("mode", initial["mode"]), ("pause",) if initial["paused"] else ("resume",)]:
            try:
                invoke(*command)
            except Exception as error:
                summary["restoration_errors"].append({"command": list(command), "error": str(error)})
        try:
            summary["final_status"] = invoke("status", "--json")
        except Exception as error:
            summary["restoration_errors"].append({"command": ["status"], "error": str(error)})
        write_json(output / "summary.json", summary)
    print(f"Report: {output / 'summary.json'}", flush=True)
    return 1 if summary["restoration_errors"] else exit_code


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"comparison: {error}", file=sys.stderr)
        raise SystemExit(1)
