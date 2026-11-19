"""Exercise maclator's launch plan (bundle resolution + GPU bridge decision) with fixtures.

Builds disposable `.app` bundles in a temp directory: a tiny arm64 Mach-O (ad-hoc signed, as
arm64 binaries must be), optionally a universal one, plus the plist/framework variants that
each decision branch cares about. Everything runs against `--dry-run`, so no guest is ever
started.

Usage: check_launch_plan.py <maclator> [--sysroot DIR] [--keep]
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("runtime", type=Path)
parser.add_argument("--sysroot", type=Path)
parser.add_argument("--keep", action="store_true", help="keep the fixture directory")
args = parser.parse_args()
runtime = args.runtime.resolve()

PROGRAM = """
#include <stdio.h>
#include <stdlib.h>
int main(void) {
    const char *mark = getenv("MACLATOR_FIXTURE_MARK");
    if (mark) { FILE *f = fopen(mark, "w"); if (f) { fputs("launched", f); fclose(f); } }
    return 0;
}
"""

XML_PLIST = (
    '<?xml version="1.0" encoding="UTF-8"?>\n'
    '<plist><dict><key>CFBundleExecutable</key><string>{exe}</string></dict></plist>\n'
)
BINARY_PLIST = b"bplist00\x00\x01\x02\x03 deliberately unparsable"

failures = []
passed = []


def check(name, condition, expected, actual):
    if condition:
        passed.append(name)
        print(f"PASS {name}", flush=True)
    else:
        failures.append(name)
        print(f"FAIL {name}: expected={expected!r} actual={actual!r}", flush=True)


def build(root, name):
    source = root / "fixture.c"
    source.write_text(PROGRAM)
    arm = root / f"{name}-arm64"
    universal = root / f"{name}-universal"
    subprocess.run(["clang", "-arch", "arm64", "-O0", "-o", str(arm), str(source)], check=True)
    subprocess.run(["codesign", "-f", "-s", "-", str(arm)], check=True, capture_output=True)
    subprocess.run(
        ["clang", "-arch", "arm64", "-arch", "x86_64", "-O0", "-o", str(universal), str(source)],
        check=True,
    )
    subprocess.run(["codesign", "-f", "-s", "-", str(universal)], check=True, capture_output=True)
    return arm, universal


def app(root, name, plist, executables, frameworks=()):
    bundle = root / f"{name}.app"
    (bundle / "Contents/MacOS").mkdir(parents=True)
    (bundle / "Contents/Info.plist").write_bytes(plist)
    for filename, payload in executables:
        target = bundle / "Contents/MacOS" / filename
        shutil.copy2(payload, target)
        target.chmod(0o755)
    for framework in frameworks:
        (bundle / "Contents/Frameworks" / framework).mkdir(parents=True)
    return bundle


def plan(target, extra=(), env=None):
    command = [str(runtime), "--dry-run", *extra, str(target)]
    if args.sysroot:
        command[1:1] = ["--sysroot", str(args.sysroot.resolve())]
    run_env = dict(os.environ, **(env or {}))
    result = subprocess.run(command, capture_output=True, text=True, env=run_env, timeout=60)
    fields = {}
    for line in result.stderr.splitlines():
        if "=" in line and not line.startswith(" "):
            key, value = line.split("=", 1)
            if key in {"target", "gpu", "source", "reason", "shim", "argv"}:
                fields[key] = value
    return result, fields


def main():
    if not runtime.is_file():
        sys.exit(f"runtime not found: {runtime}")
    root = Path(tempfile.mkdtemp(prefix="maclator-launch-plan-"))
    try:
        arm, universal = build(root, "fixture")
        mark = root / "launched"
        env = {"MACLATOR_FIXTURE_MARK": str(mark)}
        electron = ("Electron Framework.framework",)

        f1 = app(root, "F1", XML_PLIST.format(exe="F1").encode(), [("F1", arm)])
        result, fields = plan(f1, env=env)
        check("F1 unique executable", fields.get("target") == str((f1 / "Contents/MacOS/F1").resolve())
              and fields.get("gpu") == "unset" and fields.get("reason") == "not-chromium",
              "target=F1, gpu=unset, reason=not-chromium", fields)
        check("F1 dry-run does not start the guest", not mark.exists(), "no marker file", mark.exists())

        f2 = app(root, "F2", BINARY_PLIST, [("F2", arm)])
        result, fields = plan(f2, env=env)
        check("F2 binary plist falls back to the only executable",
              fields.get("target") == str((f2 / "Contents/MacOS/F2").resolve()),
              "target=F2", fields)

        f3 = app(root, "F3", XML_PLIST.format(exe="Chosen").encode(), [("Other", arm), ("Chosen", arm)])
        result, fields = plan(f3, env=env)
        check("F3 XML plist picks CFBundleExecutable",
              fields.get("target") == str((f3 / "Contents/MacOS/Chosen").resolve()),
              "target=Chosen", fields)

        f4 = app(root, "F4", BINARY_PLIST, [("One", arm), ("Two", arm)])
        result, _ = plan(f4, env=env)
        check("F4 ambiguous bundle fails with a way out",
              result.returncode != 0 and "several executables" in result.stderr
              and "Contents/MacOS" in result.stderr,
              "non-zero exit listing candidates and the explicit path", (result.returncode, result.stderr[-200:]))

        f5 = app(root, "F5", XML_PLIST.format(exe="F5").encode(), [("F5", arm)], frameworks=electron)
        result, fields = plan(f5, env=env)
        notice = "maclator: chromium-family app; enabling Metal bridge (--no-gpu to disable)"
        check("F5 chromium family auto-enables the bridge",
              fields.get("gpu") == "scoped" and fields.get("source") == "auto"
              and fields.get("reason") == "chromium-family",
              "gpu=scoped, source=auto, reason=chromium-family", fields)
        check("F5 notice is fixed", result.stderr.count(notice) == 1, "exactly one notice line", result.stderr.count(notice))
        check("F5 empty argv is an empty field", fields.get("argv") == "", "argv=", fields.get("argv"))

        f6 = app(root, "F6", XML_PLIST.format(exe="F6").encode(), [("F6", arm)],
                 frameworks=("Squirrel.framework", "Foo.framework"))
        result, fields = plan(f6, env=env)
        check("F6 near-miss frameworks do not trigger",
              fields.get("gpu") == "unset" and fields.get("reason") == "not-chromium"
              and notice not in result.stderr,
              "gpu=unset, reason=not-chromium, no notice", (fields, notice in result.stderr))

        f7 = app(root, "F7", XML_PLIST.format(exe="F7").encode(), [("F7", universal)], frameworks=electron)
        result, fields = plan(f7, env=env)
        check("F7 universal target is untouched",
              fields.get("gpu") == "unset" and fields.get("reason") == "not-arm64",
              "gpu=unset, reason=not-arm64", fields)

        result, fields = plan(f5 / "Contents/MacOS/F5", env=env)
        check("F8 bare executable keeps the historical behaviour",
              fields.get("gpu") == "unset" and fields.get("reason") == "not-in-bundle"
              and notice not in result.stderr,
              "gpu=unset, reason=not-in-bundle, no notice", (fields, notice in result.stderr))

        result, fields = plan(f5, extra=("--no-gpu",), env=env)
        check("explicit opt-out wins over detection",
              fields.get("gpu") == "off" and fields.get("source") == "explicit"
              and notice not in result.stderr,
              "gpu=off, source=explicit, no notice", fields)

        result, fields = plan(f5, extra=("--no-gpu",), env={**env, "MACLATOR_GPU": "1"})
        check("--no-gpu beats an inherited MACLATOR_GPU",
              fields.get("gpu") == "off" and fields.get("source") == "explicit",
              "gpu=off, source=explicit", fields)

        result, fields = plan(f5, extra=("--gpu",), env=env)
        check("--gpu still means every guest process",
              fields.get("gpu") == "all" and fields.get("source") == "explicit"
              and notice not in result.stderr,
              "gpu=all, source=explicit, no notice", fields)

        result, fields = plan(f5, extra=("--gpu", "--gpu-only", "--type=gpu-process"), env=env)
        check("--gpu --gpu-only keeps the historical scoped form",
              fields.get("gpu") == "scoped" and fields.get("source") == "explicit",
              "gpu=scoped, source=explicit", fields)

        result, fields = plan(f5 / "Contents/MacOS/F5", extra=("--gpu-process",), env=env)
        check("--gpu-process scopes the bridge on the old form",
              fields.get("gpu") == "scoped" and fields.get("source") == "explicit",
              "gpu=scoped, source=explicit", fields)

        result, fields = plan(f5 / "Contents/MacOS/F5", extra=("--gpu-only", "--type=gpu-process"), env=env)
        check("--gpu-only alone does not enable the bridge on the old form",
              fields.get("gpu") == "unset" and fields.get("reason") == "not-in-bundle",
              "gpu=unset, reason=not-in-bundle", fields)

        result, fields = plan(Path("/tmp/not-in-a-bundle"))
        check("bare binary outside a bundle reports not-in-bundle",
              fields.get("reason") == "not-in-bundle", "reason=not-in-bundle", fields)
    finally:
        if args.keep:
            print(f"fixtures kept in {root}")
        else:
            shutil.rmtree(root, ignore_errors=True)

    print(f"\n{len(passed)} passed, {len(failures)} failed")
    if failures:
        print("failed: " + ", ".join(failures))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
