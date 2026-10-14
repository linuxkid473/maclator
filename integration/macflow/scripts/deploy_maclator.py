#!/usr/bin/env python3
"""Local deployment tooling, not part of the Rust resident product.

Deploy only the runtime, native proxy launcher and two known wrappers. Original
ARM applications, GPU libraries, sysroot and integration profiles are preserved.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT.parents[1]
SUPPORT = Path.home() / "Library/Application Support/Maclator"


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def replace_file(source, target):
    temp = target.with_name(target.name + ".macflow-stage")
    shutil.copy2(source, temp)
    os.replace(temp, target)


def restore(backup):
    manifest = json.loads((backup / "manifest.json").read_text())
    # All backups must be verified before replacing the first installed file.
    for entry in manifest["files"]:
        if digest(backup / entry["backup"]) != entry["original_sha256"]:
            raise RuntimeError("backup hash mismatch")
    for entry in manifest["files"]:
        replace_file(backup / entry["backup"], Path(entry["target"]))
    subprocess.run(["/usr/bin/python3", str(SUPPORT / "maclator_agent.py"), "scan", "--quiet"], check=True, timeout=120)
    print(json.dumps({"restored": str(backup)}))


def deploy():
    config = json.loads((SUPPORT / "integration.json").read_text())
    runtime = Path(config["runtime"])
    launcher = Path(config["launcher"])
    new_runtime = SOURCE / "target/release/maclator"
    new_launcher = SOURCE / "integration/build/MaclatorLauncher"
    subprocess.run(["clang", "-arch", "x86_64", "-fobjc-arc", "-O2", "-framework", "AppKit", "-framework", "Carbon", str(SOURCE / "integration/launcher.m"), "-o", str(new_launcher)], check=True)
    subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(new_launcher)], check=True)
    if not new_runtime.is_file():
        raise RuntimeError("validated release runtime is missing")
    backup = SUPPORT / "Backups" / ("macflow-adapter-" + str(time.time_ns()))
    backup.mkdir(parents=True)
    manifest = {"schema": 1, "files": [], "scope": "Maclator runtime / proxy launcher / wrappers only"}
    replacements = [(new_runtime, runtime), (new_launcher, launcher)]
    for name in ["xcodebuild-maclator", "swiftc-maclator"]:
        target = Path.home() / "bin" / name
        text = target.read_text()
        expected = "exec /usr/bin/nice -n 10 "
        if expected not in text:
            raise RuntimeError("wrapper differs from reviewed baseline: " + name)
        staged = backup / (name + ".new")
        staged.write_text(text.replace(expected, "exec ", 1))
        staged.chmod(target.stat().st_mode & 0o777)
        replacements.append((staged, target))
    for index, (_, target) in enumerate(replacements):
        saved = str(index) + "-" + target.name
        shutil.copy2(target, backup / saved)
        manifest["files"].append({"target": str(target), "backup": saved, "original_sha256": digest(target)})
    (backup / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    try:
        for (source, target), entry in zip(replacements, manifest["files"]):
            replace_file(source, target)
            entry["deployed_sha256"] = digest(target)
        (backup / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
        subprocess.run(["/usr/bin/python3", str(SUPPORT / "maclator_agent.py"), "scan", "--quiet"], check=True, timeout=120)
    except BaseException:
        restore(backup)
        raise
    print(json.dumps({"backup": str(backup), "files": manifest["files"]}, indent=2))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--restore", type=Path)
    args = parser.parse_args()
    if os.geteuid() == 0:
        raise SystemExit("Run as the logged-in user; no root deployment is needed")
    restore(args.restore) if args.restore else deploy()
