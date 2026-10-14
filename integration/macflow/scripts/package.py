#!/usr/bin/env python3
"""Build-only packager. All installed MacFlow executables are Rust binaries."""
from __future__ import annotations
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile

VERSION = "0.1.0"
BINARIES = {
    "macflow-ui": ("MacFlowUI", "org.macflow.app"),
    "macflow": ("macflow", "org.macflow.cli"),
    "macflow-agent": ("macflow-agent", "org.macflow.agent"),
    "macflow-helper": ("macflow-helper", "org.macflow.helper"),
}

def run(*args: str) -> str:
    result = subprocess.run(args, capture_output=True, text=True, check=False)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr.strip()}")
    return result.stdout.strip()

def write_plist(path: Path, data: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wb") as stream:
        plistlib.dump(data, stream, sort_keys=True)

def package(release_dir: Path, output: Path) -> dict:
    release_dir = release_dir.resolve()
    output = output.absolute()
    if output.name != "MacFlow.app":
        raise ValueError("output must name MacFlow.app")
    if Path("/Applications") in output.parents or Path("/Library") in output.parents:
        raise ValueError("packaging does not deploy; choose a build artifact directory")
    if "debug" in release_dir.parts:
        raise ValueError("debug binaries cannot be packaged for release")
    if len({destination.casefold() for destination, _identifier in BINARIES.values()}) != len(BINARIES):
        raise ValueError("bundle executable names collide on case-insensitive APFS")
    source_inodes = set()
    for name in BINARIES:
        source = release_dir / name
        if not source.is_file():
            raise ValueError(f"missing release binary: {source}; run cargo build --release --locked first")
        stat = source.stat()
        identity = (stat.st_dev, stat.st_ino)
        if identity in source_inodes:
            raise ValueError("release binaries alias the same inode; rebuild with distinct case-insensitive names")
        source_inodes.add(identity)
        description = run("/usr/bin/file", "-b", str(source))
        if "Mach-O" not in description or "x86_64" not in description:
            raise ValueError(f"{name} must be native x86_64 Mach-O: {description}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".macflow-package-", dir=output.parent) as temporary:
        bundle = Path(temporary) / "MacFlow.app"
        macos = bundle / "Contents" / "MacOS"
        macos.mkdir(parents=True)
        for name, (destination, identifier) in BINARIES.items():
            target = macos / destination
            shutil.copy2(release_dir / name, target)
            target.chmod(0o755)
            run("/usr/bin/codesign", "--force", "--sign", "-", "--identifier", identifier, str(target))
            run("/usr/bin/codesign", "--verify", "--strict", str(target))
        write_plist(bundle / "Contents" / "Info.plist", {
            "CFBundleIdentifier": "org.macflow.app",
            "CFBundleName": "MacFlow",
            "CFBundleDisplayName": "MacFlow",
            "CFBundleExecutable": "MacFlowUI",
            "CFBundleVersion": VERSION,
            "CFBundleShortVersionString": VERSION,
            "CFBundlePackageType": "APPL",
            "LSUIElement": True,
            "LSMinimumSystemVersion": "26.0",
            "NSPrincipalClass": "NSApplication",
            "NSHighResolutionCapable": True,
            "NSRequiresAquaSystemAppearance": False,
            "LSApplicationCategoryType": "public.app-category.utilities",
        })
        (bundle / "Contents" / "PkgInfo").write_bytes(b"APPL????")
        # SMAppService resolves BundleProgram relative to the enclosing bundle.
        # Log destinations are chosen by Rust from the target user's support
        # directory; launchd plists contain no baked-in home or global log path.
        common = {"RunAtLoad": True, "KeepAlive": True, "ThrottleInterval": 10}
        write_plist(bundle / "Contents" / "Library" / "LaunchAgents" / "org.macflow.agent.plist", {
            **common,
            "Label": "org.macflow.agent",
            "BundleProgram": "Contents/MacOS/macflow-agent",
            "MachServices": {"org.macflow.agent": True},
            "LimitLoadToSessionType": "Aqua",
            "ProcessType": "Background",
        })
        write_plist(bundle / "Contents" / "Library" / "LaunchDaemons" / "org.macflow.helper.plist", {
            **common,
            "Label": "org.macflow.helper",
            "BundleProgram": "Contents/MacOS/macflow-helper",
            "MachServices": {"org.macflow.helper": True},
            "UserName": "root",
            "ProcessType": "Background",
        })
        run("/usr/bin/codesign", "--force", "--sign", "-", "--identifier", "org.macflow.app", str(bundle))
        run("/usr/bin/codesign", "--verify", "--deep", "--strict", str(bundle))
        for destination, _identifier in BINARIES.values():
            run("/usr/bin/codesign", "--verify", "--strict", str(macos / destination))
        manifest = {
            "version": VERSION,
            "release_state": "experimental; acceptance and soak tests pending",
            "architecture": "x86_64",
            "transport": "authenticated public XPC C API implemented in Rust",
            "sha256": {destination: hashlib.sha256((macos / destination).read_bytes()).hexdigest() for destination, _identifier in BINARIES.values()},
        }
        previous = output.with_name(".MacFlow.previous-package.app")
        if previous.exists():
            shutil.rmtree(previous)
        if output.exists():
            if output.is_symlink():
                raise ValueError("refusing to replace symlink output")
            output.rename(previous)
        try:
            bundle.rename(output)
        except BaseException:
            if previous.exists() and not output.exists():
                previous.rename(output)
            raise
        if previous.exists():
            shutil.rmtree(previous)
        output.with_suffix(".build.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        return manifest

def main() -> None:
    root = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release-dir", type=Path, default=root / "target" / "release")
    parser.add_argument("--output", type=Path, default=root / "dist" / "MacFlow.app")
    args = parser.parse_args()
    try:
        manifest = package(args.release_dir, args.output)
    except (ValueError, RuntimeError, OSError) as error:
        parser.exit(1, f"MacFlow package: {error}\n")
    print(json.dumps({"bundle": str(args.output.absolute()), **manifest}, indent=2))

if __name__ == "__main__":
    main()
