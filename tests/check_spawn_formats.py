"""Exercise real ARM children in FAT32/FAT64 and both byte orders, plus late x86."""
import argparse
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile


def fat(slices, wide, endian):
    stride = 32 if wide else 20
    offset = (8 + len(slices) * stride + 16383) & ~16383
    table = bytearray(struct.pack(endian + "II", 0xcafebabf if wide else 0xcafebabe, len(slices)))
    body = bytearray(offset)
    for cpu, subtype, payload in slices:
        table.extend(struct.pack(endian + ("IIQQII" if wide else "IIIII"),
            cpu, subtype, offset, len(payload), 14, *([0] if wide else [])))
        body.extend(payload)
        offset = (len(body) + 16383) & ~16383
        body.extend(b"\0" * (offset - len(body)))
    body[:len(table)] = table
    return body


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--runtime", required=True)
    p.add_argument("--sysroot", required=True)
    p.add_argument("--output", type=Path, required=True)
    a = p.parse_args()
    a.output.mkdir(parents=True, exist_ok=True)
    result = {}
    with tempfile.TemporaryDirectory(prefix="maclator-fat-check-") as tmp:
        root = Path(tmp)
        source = Path(__file__).parent / "guest/spawn_formats.c"
        guests = {}
        for arch in ["arm64", "x86_64"]:
            exe = root / arch
            subprocess.run(["/usr/bin/clang", "-arch", arch, "-O2", str(source), "-o", str(exe)], check=True)
            subprocess.run(["/usr/bin/codesign", "-f", "-s", "-", str(exe)], check=True, capture_output=True)
            payload = exe.read_bytes()
            cpu, subtype = struct.unpack_from("<II", payload, 4)
            guests[arch] = (cpu, subtype, payload)
        checks = [("thin-arm", root / "arm64", "arm64")]
        for wide in [False, True]:
            for endian in [">", "<"]:
                name = f"fat{64 if wide else 32}-{'be' if endian == '>' else 'le'}-arm"
                exe = root / name
                exe.write_bytes(fat([guests["arm64"]], wide, endian))
                exe.chmod(0o755)
                checks.append((name, exe, "arm64"))
        # Eight other architectures before the native slice reproduce the old
        # first-eight-entries routing bug without assuming the child's ISA.
        dummy = [(0x100 + i, 0, bytes(32)) for i in range(8)]
        exe = root / "late-native"
        exe.write_bytes(fat([guests["arm64"]] + dummy + [guests["x86_64"]], False, ">"))
        exe.chmod(0o755)
        checks.append(("late-native", exe, "x86_64"))
        for name, child_path, expected in checks:
            command = [a.runtime, "--no-aot", "--sysroot", a.sysroot, str(root / "arm64"), str(child_path)]
            with (a.output / (name + ".txt")).open("w") as log:
                child = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    code = child.wait(timeout=25)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
                    raise RuntimeError(name + " timed out")
            text = (a.output / (name + ".txt")).read_text()
            passed = code == 0 and f"child-architecture={expected}" in text
            result[name] = dict(exit_code=code, expected_architecture=expected, passed=passed)
            (a.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
            print(name, "PASS" if passed else "FAIL", flush=True)
            if not passed: raise RuntimeError(name + " failed:\n" + text[-4000:])


if __name__ == "__main__":
    main()
