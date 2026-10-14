"""Bounded parser, source preservation, bundle ownership and update-lock tests."""
import fcntl
import hashlib
from pathlib import Path
import plistlib
import shutil
import struct
import subprocess
import tempfile
import unittest

import maclator_agent as agent


def thin(cpu=agent.ARM64, endian="<", encrypted=False, subtype=0):
    command = struct.pack(endian + "6I", 0x2C, 24, 0, 1, int(encrypted), 0) if encrypted else b""
    return struct.pack(endian + "8I", 0xFEEDFACF, cpu, subtype, 2, int(encrypted), len(command), 0, 0) + command


def fat(cpus, endian=">", width=20):
    magic = 0xCAFEBABE if width == 20 else 0xCAFEBABF
    slices = [thin(cpu) for cpu in cpus]
    offset = 8 + len(cpus) * width
    table = b""
    for cpu, data in zip(cpus, slices):
        fields = (cpu, 2, offset, len(data), 0) if width == 20 else (cpu, 2, offset, len(data), 0, 0)
        table += struct.pack(endian + ("5I" if width == 20 else "IIQQII"), *fields)
        offset += len(data)
    return struct.pack(endian + "2I", magic, len(cpus)) + table + b"".join(slices)


class DiscoveryTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="maclator-discovery-test-")
        self.base = Path(self.temporary.name)
        self.previous_support = agent.SUPPORT
        self.previous_state = agent.STATE
        agent.SUPPORT = self.base / "support"
        agent.STATE = agent.SUPPORT / "state.json"

    def tearDown(self):
        for app in self.base.rglob("*.app"):
            if agent.managed(app) and agent.LSREGISTER.exists():
                subprocess.run([str(agent.LSREGISTER), "-u", str(app)], capture_output=True)
        agent.SUPPORT = self.previous_support
        agent.STATE = self.previous_state
        self.temporary.cleanup()

    def classify(self, data):
        path = self.base / "binary"
        path.write_bytes(data)
        return agent.arm_only(path)

    def test_thin_endian_cpu_and_encryption(self):
        for endian in ("<", ">"):
            self.assertTrue(self.classify(thin(endian=endian)))
            self.assertFalse(self.classify(thin(agent.X86_64, endian)))
            self.assertFalse(self.classify(thin(endian=endian, encrypted=True)))
            self.assertFalse(self.classify(thin(endian=endian, subtype=2)))

    def test_all_fat_variants_and_late_native_slice(self):
        for endian in ("<", ">"):
            for width in (20, 32):
                self.assertTrue(self.classify(fat([agent.ARM64], endian, width)))
                self.assertFalse(self.classify(fat([agent.ARM64] * 9 + [agent.X86_64], endian, width)))
                self.assertFalse(self.classify(fat([0x12], endian, width)))

    def test_bad_ranges_and_commands_rejected(self):
        with self.assertRaises(ValueError):
            self.classify(fat([agent.ARM64])[:-1])
        with self.assertRaises(ValueError):
            self.classify(thin(encrypted=True)[:-1])
        self.assertFalse(self.classify(b""))
        for kind in (1, 6, 8):
            data = bytearray(thin())
            struct.pack_into("<I", data, 12, kind)
            self.assertFalse(self.classify(data))

    def test_no_embedded_helpers_or_proxy_recursion(self):
        parent = self.base / "Main.app"
        (parent / "Contents/Helpers/Nested.app").mkdir(parents=True)
        proxy = self.base / "Proxies"
        (proxy / "Entry.app").mkdir(parents=True)
        found = list(agent.candidates([dict(path=str(self.base), depth=4)], [proxy.resolve()]))
        self.assertEqual(found, [parent.resolve()])

    def fixture(self):
        app = self.base / "Original.app"
        (app / "Contents/MacOS").mkdir(parents=True)
        executable = app / "Contents/MacOS/Original"
        executable.write_bytes(thin())
        executable.chmod(0o755)
        info = dict(CFBundleExecutable="Original", CFBundleName="Original", LSMinimumSystemVersion="26.0")
        with (app / "Contents/Info.plist").open("wb") as stream:
            plistlib.dump(info, stream)
        config = dict(runtime="/usr/local/bin/maclator", sysroot="/unused", app_icon=False,
                      launcher=str(Path(__file__).parent / "build/MaclatorLauncher"),
                      proxies=str(self.base / "Proxies"), profiles={})
        return app, info, executable, config

    def test_source_unchanged_and_running_proxy_update_deferred(self):
        app, info, executable, config = self.fixture()
        before = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in app.rglob("*") if p.is_file()}
        proxy, changed = agent.make_proxy(app, info, executable, None, config)
        self.assertTrue(changed)
        after = {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in app.rglob("*") if p.is_file()}
        self.assertEqual(before, after)
        proxy_info = Path(proxy) / "Contents/Info.plist"
        original_metadata = proxy_info.read_bytes()
        key = hashlib.sha256(str(Path(proxy).resolve()).encode()).hexdigest()
        with (agent.SUPPORT / "Locks" / (key + ".lock")).open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_SH)
            config["profiles"][str(app)] = dict(arguments=["--interp"])
            self.assertEqual(agent.make_proxy(app, info, executable, None, config), (proxy, False))
            self.assertEqual(proxy_info.read_bytes(), original_metadata)
        self.assertEqual(agent.make_proxy(app, info, executable, None, config), (proxy, True))
        self.assertNotEqual(proxy_info.read_bytes(), original_metadata)

    def test_unmanaged_destination_preserved(self):
        app, info, executable, config = self.fixture()
        original = Path(config["proxies"]) / "Original (Maclator).app"
        original.mkdir(parents=True)
        (original / "user-file").write_text("keep")
        proxy, changed = agent.make_proxy(app, info, executable, None, config)
        self.assertNotEqual(Path(proxy), original)
        self.assertEqual((original / "user-file").read_text(), "keep")
        self.assertTrue(changed)

    def test_malformed_metadata_does_not_end_scan(self):
        app, info, executable, config = self.fixture()
        config["roots"] = [dict(path=str(self.base), depth=1)]
        invalid = self.base / "Invalid.app/Contents"
        invalid.mkdir(parents=True)
        with (invalid / "Info.plist").open("wb") as stream:
            plistlib.dump(["invalid root shape"], stream)
        state = agent.scan(config)
        self.assertEqual(len(state["applications"]), 1)
        self.assertEqual(len(state["errors"]), 1)
        metadata = self.base / "Broken.app/Contents"
        metadata.mkdir(parents=True)
        with (metadata / "Info.plist").open("wb") as stream:
            plistlib.dump(dict(CFBundleIdentifier=42, MaclatorGenerated=1), stream)
        self.assertFalse(agent.managed(metadata.parent))

    def test_deleted_source_pruned_without_touching_other_bundle(self):
        app, info, executable, config = self.fixture()
        proxy, changed = agent.make_proxy(app, info, executable, None, config)
        shutil.rmtree(app)
        removed = agent.prune_proxies(config, [], set())
        self.assertEqual(removed, [proxy])
        self.assertFalse(Path(proxy).exists())


if __name__ == "__main__":
    unittest.main()
