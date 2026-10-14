"""Regression checks for the combined runtime, each with a process-group deadline."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", required=True)
    parser.add_argument("--icon", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    base = Path(__file__).resolve().parent.parent
    runtime = str(Path(args.runtime).resolve())
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    results = {}
    with tempfile.TemporaryDirectory(prefix="maclator-overlay-regression-") as directory:
        overlay = Path(directory)
        probe = overlay / "Library/Developer/PrivateFrameworks/MaclatorProbe.framework"
        probe.mkdir(parents=True)
        (probe / "probe.txt").write_bytes(b"overlay")
        (probe / "relative-link").symlink_to("probe.txt")
        (overlay / "System/Library/CoreServices/CoreTypes.bundle/Contents").mkdir(parents=True)
        checks = [
            ("branchcache", [runtime, "--selftest-jit", "branchcache", "12000"]),
            ("thread", [runtime, "--no-aot", str(base / "tests/guest/thread_create")]),
            ("spawn", ["/usr/bin/python3", str(base / "tests/check_spawn_options.py"), runtime]),
            ("overlay", [runtime, "--no-aot", "--overlay-root", str(overlay), str(base / "tests/guest/overlay_paths")]),
            ("icon-spawn", [runtime, "--no-aot", "--app-icon", args.icon, str(base / "integration/build/test_icon_spawn")]),
        ]
        for name, command in checks:
            with (output / (name + ".txt")).open("w") as log:
                child = subprocess.Popen(["/usr/bin/nice", "-n", "10"] + command,
                                         stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = child.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
                    raise RuntimeError(name + " timed out")
            results[name] = dict(exit_code=code, result="PASS" if code == 0 else "FAIL")
            (output / "runtime-result.json").write_text(json.dumps(results, indent=2) + "\n")
            if code:
                raise RuntimeError(name + " failed:\n" + (output / (name + ".txt")).read_text()[-2000:])
            print(name + ": PASS", flush=True)


if __name__ == "__main__":
    main()
