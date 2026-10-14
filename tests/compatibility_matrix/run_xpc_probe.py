#!/usr/bin/env python3
"""Construct and test our own bundles under output; no original app mutation."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import signal
import subprocess
import time
import uuid

spec = importlib.util.spec_from_file_location("probes", Path(__file__).with_name("run_probes.py"))
probes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probes)


def bundle(directory, kind, ident, executable):
    contents = directory / "Contents"
    (contents / "MacOS").mkdir(parents=True)
    info = dict(CFBundleIdentifier=ident, CFBundleName=directory.stem,
                CFBundleExecutable=executable, CFBundlePackageType=kind,
                CFBundleVersion="1")
    if kind == "XPC!":
        info["XPCService"] = dict(ServiceType="Application", RunLoopType="NSRunLoop")
    with (contents / "Info.plist").open("wb") as f:
        plistlib.dump(info, f)
    return contents / "MacOS" / executable


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--runtime")
    p.add_argument("--output")
    p.add_argument("--sysroot", help="ARM sysroot; required when building a probe set")
    p.add_argument("--wrap-arm-services", action="store_true",
                   help="add self-owned ARM services with a native execl entry")
    p.add_argument("--prepare-only", action="store_true",
                   help="only build/sign self-owned bundles; do not launch clients or services")
    p.add_argument("--run-prepared", help="run an existing results.json without rebuilding")
    a = p.parse_args()
    if a.run_prepared:
        if a.runtime or a.output or a.prepare_only or a.wrap_arm_services:
            p.error("--run-prepared uses its saved paths and cannot be combined with build options")
        manifest = Path(a.run_prepared).resolve()
        result = json.loads(manifest.read_text())
        runtime = Path(result["runtime"])
        if hashlib.sha256(runtime.read_bytes()).hexdigest() != result["runtime_sha256"]:
            raise RuntimeError("prepared runtime changed; build a new probe set")
        for case, row in result["tests"].items():
            if "run_command" in row:
                run_case(case, row, manifest.parent, str(runtime))
                manifest.write_text(json.dumps(result, indent=2) + "\n")
        result["prepared_only"] = False
        manifest.write_text(json.dumps(result, indent=2) + "\n")
        return
    if not a.runtime or not a.output or not a.sysroot:
        p.error("building requires --runtime, --output and --sysroot")
    output = Path(a.output).resolve()
    output.mkdir(parents=True, exist_ok=True)
    token = uuid.uuid4().hex
    requested_runtime = Path(a.runtime).resolve()
    runtime = str(requested_runtime)
    if a.prepare_only or a.wrap_arm_services:
        # Preserve this exact build for the later test window; native wrappers
        # compile in this path and must not silently use a newer runtime.
        snapshot = output / "Runtime/maclator"
        snapshot.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(requested_runtime, snapshot)
        runtime = str(snapshot)
    sysroot = str(Path(a.sysroot).resolve())
    result = dict(runtime=runtime, runtime_sha256=hashlib.sha256(Path(runtime).read_bytes()).hexdigest(),
                  requested_runtime=str(requested_runtime), sysroot=sysroot, prepared_only=a.prepare_only,
                  scope="self-owned in-bundle NSXPC echo; direct executable launch; optional native service entry",
                  tests={})
    source = str(Path(__file__).with_name("xpc_probe.m"))
    env = dict(os.environ)
    cases = [("x86_64", "x86_64", False), ("arm64", "arm64", False),
             ("arm64", "x86_64", False), ("x86_64", "arm64", False)]
    if a.wrap_arm_services:
        cases += [("arm64", "arm64", True), ("x86_64", "arm64", True)]
    for client_arch, service_arch, wrapped in cases:
        case = client_arch + "-client-" + service_arch + "-service"
        if wrapped:
            case += "-via-native-wrapper"
        ident = "org.maclator.compatibility." + token + "." + case
        app = output / ("XPCProbe-" + case + ".app")
        client = bundle(app, "APPL", ident, "XPCProbe")
        service_id = ident + ".Echo"
        service = bundle(app / "Contents/XPCServices/Echo.xpc", "XPC!", service_id, "Echo")
        service_log = str(output / (case + "-service.txt"))
        payload = service.with_name("Echo-arm64") if wrapped else service
        checks = [("client", client_arch, [], client),
                  ("service", service_arch,
                   ["-DMCL_XPC_SERVICE", "-DMCL_XPC_LOG=" + json.dumps(service_log)], payload)]
        builds = []
        for name, arch, flags, binary in checks:
            command = ["/usr/bin/clang", "-arch", arch, "-O2", "-fobjc-arc", "-fblocks",
                       "-framework", "Foundation"] + flags + [source, "-o", str(binary)]
            builds.append(probes.run(command, output / (case + "-" + name + "-build.txt"), 25, env))
        if wrapped:
            definitions = dict(MCL_XPC_RUNTIME=runtime, MCL_XPC_SYSROOT=sysroot,
                               MCL_XPC_PAYLOAD=str(payload), MCL_XPC_LOG=service_log)
            command = ["/usr/bin/clang", "-arch", "x86_64", "-O2", "-std=c11", "-Wall", "-Wextra"]
            command += ["-D" + name + "=" + json.dumps(value) for name, value in definitions.items()]
            command += [str(Path(__file__).with_name("xpc_native_launcher.c")), "-o", str(service)]
            builds.append(probes.run(command, output / (case + "-wrapper-build.txt"), 25, env))
        if any(b["exit_code"] for b in builds):
            result["tests"][case] = dict(builds=builds, result="BUILD_FAILED")
            continue
        signs = [probes.run(["/usr/bin/codesign", "--force", "--sign", "-", str(path)],
                            output / (case + "-codesign-" + str(i) + ".txt"), 10, env)
                 for i, path in enumerate([service.parent.parent.parent, app])]
        if any(s["exit_code"] for s in signs):
            result["tests"][case] = dict(builds=builds, signs=signs, result="SIGN_FAILED")
            continue
        command = [str(client), service_id]
        if client_arch == "arm64":
            command = [runtime, "--no-aot"] + command
        row = dict(builds=builds, signs=signs, run_command=command, service_log=service_log,
                   wrapped=wrapped, service_payload=str(payload), result="PREPARED")
        result["tests"][case] = row
        if not a.prepare_only:
            run_case(case, row, output, runtime)
        else:
            print(case + ": PREPARED (not launched)", flush=True)
        (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")
    (output / "results.json").write_text(json.dumps(result, indent=2) + "\n")


def owned_services(row, output, runtime):
    """Match only this generated bundle's exact executable or translated payload."""
    payload = Path(row["service_payload"]).resolve()
    if output.resolve() not in payload.parents:
        raise RuntimeError("refusing service cleanup outside owned output")
    executable = str(payload.with_name("Echo") if row["wrapped"] else payload)
    snapshot = subprocess.run(["/bin/ps", "-ww", "-axo", "pid=,command="],
                              capture_output=True, text=True, timeout=2, check=True)
    found = []
    for line in snapshot.stdout.splitlines():
        fields = line.strip().split(None, 1)
        if len(fields) != 2 or not fields[0].isdigit():
            continue
        pid, command = int(fields[0]), fields[1]
        native = command == executable or command.startswith(executable + " ")
        translated = command.startswith(runtime + " ") and (" " + str(payload) + " ") in (command + " ")
        if pid != os.getpid() and (native or translated):
            found.append(dict(pid=pid, command=command))
    return found


def cleanup_services(row, output, runtime):
    # launchd services do not necessarily share the client's process group.
    # Re-read exact owned commands before each signal to avoid killing other apps.
    cleanup = dict(term=[], kill=[], remaining=[])
    for child in owned_services(row, output, runtime):
        try:
            os.kill(child["pid"], signal.SIGTERM)
            cleanup["term"].append(child)
        except ProcessLookupError:
            pass
    if cleanup["term"]:
        time.sleep(0.25)
    for child in owned_services(row, output, runtime):
        try:
            os.kill(child["pid"], signal.SIGKILL)
            cleanup["kill"].append(child)
        except ProcessLookupError:
            pass
    if cleanup["kill"]:
        time.sleep(0.1)
    cleanup["remaining"] = owned_services(row, output, runtime)
    return cleanup


def run_case(case, row, output, runtime):
    env = dict(os.environ)
    log = Path(row["service_log"])
    offset = log.stat().st_size if log.exists() else 0
    row["service_log_offset"] = offset
    check = probes.run(row["run_command"], output / (case + "-echo.txt"), 15, env)
    check["result"] = "TIMEOUT" if check["timeout"] else "PASS" if not check["exit_code"] else "FAIL"
    row["probe"] = check
    row["result"] = check["result"]
    row["service_cleanup"] = cleanup_services(row, output, runtime)
    row["service_log_exists"] = log.exists()
    if log.exists():
        row["service_log_text"] = log.read_bytes()[offset:].decode(errors="replace")
    print(case + ": " + check["result"], flush=True)


if __name__ == "__main__":
    main()
