#!/usr/bin/env python3
"""Pure warmup-parser self-check and optional old-log inspection; no runtime/probe/build invocation."""
import argparse
import importlib.util
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stdout", type=Path, action="append", default=[])
    args = parser.parse_args()
    path = Path(__file__).with_name("run_presentation_probe_v31.py")
    spec = importlib.util.spec_from_file_location("v31_repro_runner", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)  # imports declarations only; module main guarded
    checks = module.warmup_parser_selfcheck()
    logs = []
    for candidate in args.stdout:
        stdout = candidate.read_text()
        begin = re.search(r"NO_MAIN_PUMP_BEGIN: requested_frames=12 pool_count=3 main_thread=1", stdout)
        logs.append({"path": str(candidate.resolve()),
                     "warmup_before_critical": bool(module.warmup_precedes_critical(stdout, begin)),
                     "scope": "warmup ordering only; not a full probe validation"})
    passed = all(check["passed"] for check in checks) and all(log["warmup_before_critical"] for log in logs)
    print(json.dumps({"data_only": True, "runtime_or_build_invoked": False, "passed": passed,
                      "parser_selfcheck": checks, "log_inspection": logs}, indent=2))
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
