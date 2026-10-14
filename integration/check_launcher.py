"""End-to-end check with a disposable ARM app, an actual native .app and open(1)."""
import argparse
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import subprocess
import tempfile
import time

import maclator_agent as agent


def wait_for(path, text, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists() and text in path.read_text():
            return
        time.sleep(0.2)
    raise RuntimeError("missing guest event: " + text + "\n" + (path.read_text() if path.exists() else "no log"))


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runtime", required=True)
    parser.add_argument("--sysroot", required=True)
    parser.add_argument("--icon", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    base = Path(__file__).resolve().parent
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="maclator-entry-check-", dir=base / "build"))
    result = {}
    proxy = None
    try:
        app = root / "LauncherGuest.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        (app / "Contents/Resources").mkdir()
        target = app / "Contents/MacOS/LauncherGuest"
        shutil.copy2(base / "build/LauncherGuest", target)
        icon = app / "Contents/Resources/AppIcon.icns"
        shutil.copy2(args.icon, icon)
        info = dict(CFBundleExecutable="LauncherGuest", CFBundleName="Maclator Entry Test",
                    CFBundleIdentifier="org.maclator.regression.guest", CFBundlePackageType="APPL",
                    CFBundleIconFile="AppIcon.icns", LSUIElement=True, LSMinimumSystemVersion="26.0")
        with (app / "Contents/Info.plist").open("wb") as stream:
            plistlib.dump(info, stream)
        subprocess.run(["/usr/bin/codesign", "--force", "--sign", "-", str(app)], check=True, capture_output=True)
        log = root / "events.log"
        config = dict(runtime=str(Path(args.runtime).resolve()), sysroot=str(Path(args.sysroot).resolve()),
                      launcher=str(base / "build/MaclatorLauncher"), proxies=str(root / "Proxies"), app_icon=True,
                      profiles={str(app): {"arguments": ["--no-aot", "--dump-on-fault"],
                                          "environment": {"MCL_TEST_LOG": str(log)}}})
        proxy, created = agent.make_proxy(app, info, target, icon, config)
        subprocess.run(["/usr/bin/open", proxy], check=True, timeout=10)
        wait_for(log, "ready icon=1 valid=1")
        result["native_open_arm_gui_icon"] = "PASS"
        # A second open targets the same launcher; it must not spawn a duplicate.
        subprocess.run(["/usr/bin/open", proxy], check=True, timeout=10)
        document = root / "hello.txt"
        document.write_text("Maclator document event regression\n")
        subprocess.run(["/usr/bin/open", "-a", proxy, str(document)], check=True, timeout=10)
        wait_for(log, "file " + str(document))
        result["explicit_open_document"] = "PASS"
        subprocess.run(["/usr/bin/open", "-a", proxy, "maclatortest://hello"], check=True, timeout=10)
        wait_for(log, "url maclatortest://hello")
        result["explicit_open_url"] = "PASS"
        config["profiles"][str(app)]["arguments"].append("--trace")
        result["running_entry_update_deferred"] = "PASS" if agent.make_proxy(app, info, target, icon, config) == (proxy, False) else "FAIL"
        if result["running_entry_update_deferred"] != "PASS":
            raise RuntimeError("native launcher did not hold its update lock")
        if log.read_text().count("started arm64") != 1:
            raise RuntimeError("duplicate guest process on reopen")
        result["reopen_no_duplicate"] = "PASS"
        # The guest exits itself after 25 seconds; no user application is stopped.
        time.sleep(1)
        (output / "entry-events.log").write_text(log.read_text())
        print(json.dumps(result, indent=2))
    finally:
        # Only terminate children whose argv points into this disposable test directory.
        listing = subprocess.check_output(["/bin/ps", "-axo", "pid=,command="], text=True)
        for line in listing.splitlines():
            fields = line.strip().split(None, 1)
            if len(fields) == 2 and str(root) in fields[1] and int(fields[0]) != os.getpid():
                try:
                    os.kill(int(fields[0]), signal.SIGTERM)
                except ProcessLookupError:
                    pass
        time.sleep(0.3)
        if proxy and agent.LSREGISTER.exists():
            subprocess.run([str(agent.LSREGISTER), "-u", proxy], capture_output=True)
        atomic = output / "entry-result.json"
        atomic.write_text(json.dumps(result, indent=2) + "\n")
        shutil.rmtree(root)


if __name__ == "__main__":
    main()
