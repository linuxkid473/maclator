#!/usr/bin/env python3
"""Read-only inventory. Mach-O architecture is evidence, not a launch test."""
import argparse
import collections
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import struct

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("discovery", ROOT / "integration/maclator_agent.py")
discovery = importlib.util.module_from_spec(spec)
spec.loader.exec_module(discovery)


def architecture(path):
    size = path.stat().st_size
    with path.open("rb") as f:
        head = f.read(8)
        magic = head[:4]
        fat = {b"\xca\xfe\xba\xbe": (">", 20), b"\xbe\xba\xfe\xca": ("<", 20),
               b"\xca\xfe\xba\xbf": (">", 32), b"\xbf\xba\xfe\xca": ("<", 32)}
        if magic not in fat:
            cpu, encrypted, kind, subtype = discovery.read_thin(f, 0, size)
            return [dict(cpu=cpu, subtype=subtype, encrypted=encrypted, filetype=kind)]
        endian, stride = fat[magic]
        count = struct.unpack(endian + "I", head[4:])[0]
        if not count or count > 65536 or 8 + count * stride > size:
            raise ValueError("invalid FAT table")
        table = f.read(count * stride)
        out = []
        for i in range(count):
            entry = table[i * stride:(i + 1) * stride]
            if stride == 20:
                cpu, subtype, offset, length, alignment = struct.unpack(endian + "5I", entry)
            else:
                cpu, subtype, offset, length, alignment, reserved = struct.unpack(endian + "IIQQII", entry)
            actual, encrypted, kind, actual_subtype = discovery.read_thin(f, offset, length)
            if cpu != actual:
                raise ValueError("FAT/table CPU mismatch")
            out.append(dict(cpu=cpu, subtype=actual_subtype, encrypted=encrypted, filetype=kind))
        return out


def scan(roots):
    entries = []
    for root in roots:
        for base, directories, files in os.walk(root):
            if "Info.plist" not in files or not base.endswith("/Contents"):
                continue
            bundle = Path(base).parent
            if bundle.suffix not in (".app", ".xpc", ".appex"):
                continue
            row = dict(bundle=str(bundle), kind=bundle.suffix)
            try:
                with (Path(base) / "Info.plist").open("rb") as f:
                    info = plistlib.load(f)
                name = info.get("CFBundleExecutable")
                row["bundle_id"] = info.get("CFBundleIdentifier")
                row["executable"] = str(Path(base) / "MacOS" / name) if name else None
                if name:
                    slices = architecture(Path(row["executable"]))
                    row["slices"] = slices
                    row["arm64"] = any(s["cpu"] == 0x0100000C for s in slices)
                    row["x86_64"] = any(s["cpu"] == 0x01000007 for s in slices)
                    row["encrypted"] = any(s["encrypted"] for s in slices)
                    row["routing"] = ("native" if row["x86_64"] else "translated-candidate"
                                      if row["arm64"] and not row["encrypted"] else "unsupported")
            except (OSError, ValueError, plistlib.InvalidFileException, struct.error, TypeError) as e:
                row["error"] = str(e)
            entries.append(row)
    return entries


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--root", action="append", required=True)
    p.add_argument("--output", required=True)
    a = p.parse_args()
    entries = scan(a.root)
    result = dict(roots=a.root, evidence="static inventory only; no application launch",
                  counts=dict(collections.Counter((e.get("routing", "unknown") + " " + e["kind"]) for e in entries)),
                  entries=entries)
    target = Path(a.output)
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps(result["counts"], ensure_ascii=False))


if __name__ == "__main__":
    main()
