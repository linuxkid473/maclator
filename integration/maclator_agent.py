#!/usr/bin/env python3
"""Discover ARM-only application bundles and create native Maclator entries.

Runs once at login and once a minute via a user LaunchAgent. No privileged
kernel hooks, bundle edits, quarantine removal, or global file associations.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shlex
import shutil
import struct
import subprocess
import sys
import tempfile
import time

SUPPORT = Path.home() / "Library/Application Support/Maclator"
CONFIG = SUPPORT / "integration.json"
STATE = SUPPORT / "applications.json"
PROXIES = Path.home() / "Applications/Maclator Apps"
LABEL = "org.maclator.application-discovery"
JOB = Path.home() / "Library/LaunchAgents" / (LABEL + ".plist")
LSREGISTER = Path("/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister")
ARM64 = 0x0100000C
X86_64 = 0x01000007


def atomic_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, ensure_ascii=False, indent=2)
            stream.write("\n")
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def read_thin(stream, offset, length):
    """Return (CPU, encrypted, filetype, subtype) for a bounded Mach-O slice."""
    if length < 32:
        raise ValueError("short Mach-O")
    stream.seek(offset)
    header = stream.read(32)
    endian = {b"\xcf\xfa\xed\xfe": "<", b"\xfe\xed\xfa\xcf": ">"}.get(header[:4])
    if endian is None:
        raise ValueError("not a 64-bit Mach-O")
    _, cpu, subtype, filetype, ncmds, sizeofcmds, _, _ = struct.unpack(endian + "8I", header)
    if sizeofcmds > length - 32 or sizeofcmds > 64 * 1024 * 1024 or ncmds > sizeofcmds // 8:
        raise ValueError("invalid Mach-O commands")
    commands = stream.read(sizeofcmds)
    cursor = 0
    encrypted = False
    for _ in range(ncmds):
        if cursor + 8 > len(commands):
            raise ValueError("short load command")
        command, size = struct.unpack_from(endian + "2I", commands, cursor)
        if size < 8 or size > len(commands) - cursor:
            raise ValueError("invalid load command size")
        if command in (0x21, 0x2C):  # LC_ENCRYPTION_INFO / LC_ENCRYPTION_INFO_64
            if size < 20:
                raise ValueError("short encryption command")
            encrypted |= struct.unpack_from(endian + "I", commands, cursor + 16)[0] != 0
        cursor += size
    return cpu, encrypted, filetype, subtype


def arm_only(path):
    """Handle both FAT widths/endian forms and all slices; skip encrypted ARM."""
    with Path(path).open("rb") as stream:
        size = os.fstat(stream.fileno()).st_size
        prefix = stream.read(8)
        if len(prefix) != 8:
            return False
        fat = {b"\xca\xfe\xba\xbe": (">", 20), b"\xbe\xba\xfe\xca": ("<", 20),
               b"\xca\xfe\xba\xbf": (">", 32), b"\xbf\xba\xfe\xca": ("<", 32)}.get(prefix[:4])
        if fat is None:
            cpu, encrypted, filetype, subtype = read_thin(stream, 0, size)
            return cpu == ARM64 and not encrypted and filetype == 2 and (subtype & 0xFF) != 2
        endian, width = fat
        count = struct.unpack(endian + "I", prefix[4:])[0]
        if count < 1 or count > 4096 or count * width > size - 8:
            raise ValueError("invalid FAT table")
        table = stream.read(count * width)
        slices = []
        for i in range(count):
            if width == 20:
                cpu, _, offset, length, _ = struct.unpack_from(endian + "5I", table, i * width)
            else:
                cpu, _, offset, length, _, _ = struct.unpack_from(endian + "IIQQII", table, i * width)
            if offset < 8 + count * width or length == 0 or offset > size or length > size - offset:
                raise ValueError("invalid FAT slice range")
            slices.append((cpu, offset, length))
        if any(cpu == X86_64 for cpu, _, _ in slices):
            return False
        arm = [entry for entry in slices if entry[0] == ARM64]
        if not arm:
            return False
        ordinary_arm = False
        for cpu, offset, length in arm:
            actual, encrypted, filetype, subtype = read_thin(stream, offset, length)
            if actual != cpu or encrypted or filetype != 2:
                return False
            ordinary_arm |= (subtype & 0xFF) != 2
        # The runtime's documented application support excludes arm64e-only.
        return ordinary_arm


def candidates(roots, exclusions):
    seen = set()
    for item in roots:
        root = Path(item["path"]).expanduser()
        maximum = int(item.get("depth", 1))
        if not root.exists():
            continue
        pending = [(root, 0)]
        while pending:
            directory, depth = pending.pop()
            resolved = directory.resolve()
            if resolved in seen or any(resolved == excluded or excluded in resolved.parents for excluded in exclusions):
                continue
            seen.add(resolved)
            if directory.suffix.lower() == ".app":
                yield resolved
                continue  # Never discover embedded helpers or frameworks.
            if depth >= maximum:
                continue
            try:
                with os.scandir(directory) as entries:
                    for entry in entries:
                        if not entry.name.startswith(".") and entry.is_dir(follow_symlinks=False):
                            pending.append((Path(entry.path), depth + 1))
            except (OSError, PermissionError):
                continue


def original_icon(app, info):
    resources = app / "Contents/Resources"
    name = info.get("CFBundleIconFile")
    if isinstance(name, str):
        for value in (name, name + ".icns"):
            path = (resources / value).resolve()
            if resources.resolve() in path.parents and path.is_file():
                return path
    # Resource-only fallback; do not ask NSWorkspace for an ARM-unsupported icon.
    for name in ("AppIcon.icns", "app.icns", "icon.icns"):
        if (resources / name).is_file():
            return resources / name
    return None


def managed(path):
    try:
        with (path / "Contents/Info.plist").open("rb") as stream:
            info = plistlib.load(stream)
        identifier = info.get("CFBundleIdentifier") if isinstance(info, dict) else None
        return isinstance(identifier, str) and identifier.startswith("org.maclator.proxy.") and info.get("MaclatorGenerated") == 1
    except (OSError, ValueError, plistlib.InvalidFileException):
        return False


def proxy_source(path):
    with (path / "Contents/Resources/MaclatorLaunch.plist").open("rb") as stream:
        metadata = plistlib.load(stream)
    source = metadata.get("SourceApp") if isinstance(metadata, dict) else None
    if not isinstance(source, str):
        raise ValueError("invalid managed launch metadata: " + str(path))
    return source


def proxy_destination(app, info, config):
    identifier = hashlib.sha256(str(app).encode()).hexdigest()[:16]
    name = str(info.get("CFBundleDisplayName") or info.get("CFBundleName") or app.stem)
    safe_name = name.replace("/", "_").replace(":", "_").strip() or app.stem
    destination = Path(config["proxies"]) / (safe_name + " (Maclator).app")
    if destination.exists():
        if not managed(destination):
            destination = destination.with_name(safe_name + " (Maclator " + identifier[:6] + ").app")
        else:
            if proxy_source(destination) != str(app):
                destination = destination.with_name(safe_name + " (Maclator " + identifier[:6] + ").app")
    # The fallback is also required to belong to this source. Never replace
    # another managed entry just because its generated filename collides.
    if destination.exists():
        if not managed(destination):
            raise ValueError("unmanaged proxy name collision: " + str(destination))
        if proxy_source(destination) != str(app):
            raise ValueError("managed proxy name collision: " + str(destination))
    return identifier, name, destination


def make_proxy(app, info, target, icon, config):
    identifier, name, destination = proxy_destination(app, info, config)
    locks = SUPPORT / "Locks"
    locks.mkdir(parents=True, exist_ok=True)
    lock_key = hashlib.sha256(str(destination.resolve()).encode()).hexdigest()
    with (locks / (lock_key + ".lock")).open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            # An entry may be supervising a user session. Update it next scan.
            return str(destination), False
        return publish_proxy(app, info, target, icon, config, identifier, name, destination)


def publish_proxy(app, info, target, icon, config, identifier, name, destination):
    profile = config.get("profiles", {}).get(str(app), {})
    arguments = ["--sysroot", config["sysroot"]]
    arguments += list(profile.get("arguments", ["--no-aot", "--dump-on-fault"]))
    if icon and config.get("app_icon", True):
        arguments += ["--app-icon", str(icon)]
    launcher_config = dict(Runtime=config["runtime"], Target=str(target), SourceApp=str(app),
                           Arguments=arguments, Environment=profile.get("environment", {}),
                           ApplicationArguments=list(profile.get("application_arguments", [])),
                           Log=str(SUPPORT / "Logs" / (identifier + ".log")))
    fingerprint_data = [launcher_config, info]
    for path in (target, app / "Contents/Info.plist", Path(config["launcher"]), icon):
        if path:
            stat = path.stat()
            fingerprint_data.append([str(path), stat.st_size, stat.st_mtime_ns])
    fingerprint = hashlib.sha256(plistlib.dumps(fingerprint_data, sort_keys=True)).hexdigest()
    if managed(destination):
        with (destination / "Contents/Info.plist").open("rb") as stream:
            if plistlib.load(stream).get("MaclatorFingerprint") == fingerprint:
                return str(destination), False
    destination.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkdtemp(prefix=".maclator-", suffix=".app", dir=destination.parent))
    old = None
    try:
        contents = temporary / "Contents"
        contents.mkdir()
        (contents / "MacOS").mkdir()
        (contents / "Resources").mkdir()
        shutil.copy2(config["launcher"], contents / "MacOS/MaclatorLauncher")
        metadata = dict(CFBundleExecutable="MaclatorLauncher", CFBundleIdentifier="org.maclator.proxy." + identifier,
                        CFBundleName=name, CFBundleDisplayName=name, CFBundlePackageType="APPL",
                        CFBundleVersion="1", CFBundleShortVersionString="1.0", LSUIElement=True,
                        MaclatorGenerated=1, MaclatorFingerprint=fingerprint,
                        NSAppleEventsUsageDescription="向由 Maclator 启动的应用传递打开文件事件。")
        # Preserve the real minimum OS requirement instead of claiming compatibility.
        if "LSMinimumSystemVersion" in info:
            metadata["LSMinimumSystemVersion"] = info["LSMinimumSystemVersion"]
        # Explicitly configured browsers can register their original URL/file
        # types. Registration alone does not change the user's default handler.
        if profile.get("register_browser_types"):
            for key in ("CFBundleURLTypes", "CFBundleDocumentTypes"):
                if key in info:
                    metadata[key] = info[key]
        if icon:
            shutil.copy2(icon, contents / "Resources/AppIcon.icns")
            metadata["CFBundleIconFile"] = "AppIcon.icns"
        with (contents / "Info.plist").open("wb") as stream:
            plistlib.dump(metadata, stream)
        with (contents / "Resources/MaclatorLaunch.plist").open("wb") as stream:
            plistlib.dump(launcher_config, stream)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(temporary)],
                       check=True, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if destination.exists():
            if not managed(destination):
                raise ValueError("refusing to replace unmanaged application: " + str(destination))
            old = destination.with_name(".maclator-old-" + identifier + ".app")
            if old.exists():
                raise ValueError("previous update backup exists: " + str(old))
            destination.rename(old)
        try:
            temporary.rename(destination)
        except BaseException:
            if old:
                old.rename(destination)
            raise
        if old:
            shutil.rmtree(old)
        if LSREGISTER.exists():
            subprocess.run([str(LSREGISTER), "-f", str(destination)], check=False, capture_output=True)
        return str(destination), True
    finally:
        if temporary.exists():
            shutil.rmtree(temporary)


def prune_proxies(config, entries, unsupported):
    expected = {entry["source"]: entry["proxy"] for entry in entries}
    removed = []
    for proxy in Path(config["proxies"]).glob("*.app"):
        if not managed(proxy):
            continue
        try:
            source = proxy_source(proxy)
            stale = not Path(source).exists() or source in unsupported
            stale |= source in expected and str(proxy) != expected[source]
            if not stale:
                continue
            key = hashlib.sha256(str(proxy.resolve()).encode()).hexdigest()
            with (SUPPORT / "Locks" / (key + ".lock")).open("a") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    continue
                if LSREGISTER.exists():
                    subprocess.run([str(LSREGISTER), "-u", str(proxy)], capture_output=True)
                shutil.rmtree(proxy)
                removed.append(str(proxy))
        except (OSError, ValueError, plistlib.InvalidFileException):
            continue
    return removed


def scan(config):
    SUPPORT.mkdir(parents=True, exist_ok=True)
    with (SUPPORT / "discovery.lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return None
        entries, errors, updated, unsupported = [], [], 0, set()
        for app in candidates(config["roots"], [Path(config["proxies"]).resolve()]):
            try:
                with (app / "Contents/Info.plist").open("rb") as stream:
                    info = plistlib.load(stream)
                if not isinstance(info, dict):
                    errors.append(dict(source=str(app), error="Info.plist is not a dictionary"))
                    continue
                executable = info.get("CFBundleExecutable")
                if not isinstance(executable, str) or "/" in executable or executable in ("", ".", ".."):
                    continue
                target = app / "Contents/MacOS" / executable
                if not target.is_file() or not os.access(target, os.X_OK):
                    continue
                if not arm_only(target):
                    unsupported.add(str(app))
                    continue
                proxy, changed = make_proxy(app, info, target, original_icon(app, info), config)
                updated += changed
                entries.append(dict(source=str(app), target=str(target), proxy=proxy))
            except (OSError, ValueError, plistlib.InvalidFileException, subprocess.CalledProcessError) as error:
                errors.append(dict(source=str(app), error=str(error)))
        (SUPPORT / "Locks").mkdir(exist_ok=True)
        removed = prune_proxies(config, entries, unsupported)
        state = dict(scanned_at=time.time(), applications=entries, updated=updated, removed=removed, errors=errors)
        atomic_json(STATE, state)
        return state


def install(args):
    base = Path(__file__).resolve().parent
    SUPPORT.mkdir(parents=True, exist_ok=True)
    binaries = SUPPORT / "bin"
    binaries.mkdir(exist_ok=True)
    shutil.copy2(base / "build/MaclatorLauncher", binaries / "MaclatorLauncher")
    if Path(__file__).resolve() != (SUPPORT / "maclator_agent.py").resolve():
        shutil.copy2(__file__, SUPPORT / "maclator_agent.py")
    config = json.loads(CONFIG.read_text()) if CONFIG.exists() else {
        "roots": [{"path": "/Applications", "depth": 2},
                  {"path": str(Path.home() / "Applications"), "depth": 2},
                  {"path": str(Path.home() / "Downloads"), "depth": 2}],
        "profiles": {}, "proxies": str(PROXIES), "app_icon": True,
    }
    config.update(runtime=str(Path(args.runtime).resolve()), sysroot=str(Path(args.sysroot).resolve()),
                  launcher=str(binaries / "MaclatorLauncher"))
    for value in args.app:
        resolved = str(Path(value).resolve())
        if not any(root["path"] == resolved for root in config["roots"]):
            config["roots"].append(dict(path=resolved, depth=0))
    if args.xcode and args.overlay:
        xcode = str(Path(args.xcode).resolve())
        config["profiles"][xcode] = {
            "arguments": ["--no-aot", "--dump-on-fault", "--overlay-root", str(Path(args.overlay).resolve())],
            "environment": {"DEVELOPER_DIR": str(Path(xcode) / "Contents/Developer")},
        }
    atomic_json(CONFIG, config)
    bindir = Path.home() / "bin"
    bindir.mkdir(exist_ok=True)
    command = bindir / "maclator-apps"
    command.write_text("#!/bin/bash\nexec /usr/bin/python3 " + shlex.quote(str(SUPPORT / "maclator_agent.py")) + " \"$@\"\n")
    command.chmod(0o755)
    JOB.parent.mkdir(parents=True, exist_ok=True)
    job = dict(Label=LABEL, ProgramArguments=["/usr/bin/python3", str(SUPPORT / "maclator_agent.py"), "scan", "--quiet"],
               RunAtLoad=True, StartInterval=60, ProcessType="Background", Nice=10,
               StandardOutPath=str(SUPPORT / "discovery.log"), StandardErrorPath=str(SUPPORT / "discovery.log"))
    with JOB.open("wb") as stream:
        plistlib.dump(job, stream)
    domain = "gui/" + str(os.getuid())
    subprocess.run(["/bin/launchctl", "bootout", domain + "/" + LABEL], capture_output=True)
    subprocess.run(["/bin/launchctl", "bootstrap", domain, str(JOB)], check=True)
    result = scan(config)
    print(json.dumps(result, ensure_ascii=False, indent=2))


def uninstall():
    subprocess.run(["/bin/launchctl", "bootout", "gui/" + str(os.getuid()) + "/" + LABEL], capture_output=True)
    JOB.unlink(missing_ok=True)
    if CONFIG.exists():
        config = json.loads(CONFIG.read_text())
        for app in Path(config["proxies"]).glob("*.app"):
            if managed(app):
                key = hashlib.sha256(str(app.resolve()).encode()).hexdigest()
                (SUPPORT / "Locks").mkdir(exist_ok=True)
                with (SUPPORT / "Locks" / (key + ".lock")).open("a") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        print("入口仍在运行，退出后再卸载：" + str(app))
                        continue
                    if LSREGISTER.exists():
                        subprocess.run([str(LSREGISTER), "-u", str(app)], capture_output=True)
                    shutil.rmtree(app)
    print("后台发现与生成的入口已移除；原应用、Maclator 和日志保留。")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    setup = commands.add_parser("install")
    setup.add_argument("--runtime", required=True)
    setup.add_argument("--sysroot", required=True)
    setup.add_argument("--app", action="append", default=[])
    setup.add_argument("--xcode")
    setup.add_argument("--overlay")
    commands.add_parser("status")
    scanner = commands.add_parser("scan")
    scanner.add_argument("--quiet", action="store_true")
    add = commands.add_parser("add")
    add.add_argument("path")
    commands.add_parser("uninstall")
    args = parser.parse_args()
    if args.command == "install":
        install(args)
    elif args.command == "uninstall":
        uninstall()
    elif args.command == "status":
        print(STATE.read_text() if STATE.exists() else "尚未扫描")
    else:
        config = json.loads(CONFIG.read_text())
        if args.command == "add":
            value = str(Path(args.path).expanduser().resolve())
            if not any(root["path"] == value for root in config["roots"]):
                config["roots"].append(dict(path=value, depth=2))
                atomic_json(CONFIG, config)
        result = scan(config)
        if result and not getattr(args, "quiet", False):
            print(json.dumps(result, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
