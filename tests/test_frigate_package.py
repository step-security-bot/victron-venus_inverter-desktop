"""Publisher staging checks without building or executing a worker."""

import importlib.util
import json
import os
import struct
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / "scripts/plugins/plugin_package.py"
SPEC = importlib.util.spec_from_file_location("plugin_package", SCRIPT)
packaging = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(packaging)


def executable_fixture(target):
    """Construct minimal executable headers; fixtures are never executed."""
    arm = target.startswith("aarch64-")
    if "apple" in target:
        header = struct.pack("<IIIIIIII", 0xFEEDFACF, 0x100000C if arm else 0x1000007,
                             0, 2, 1, 24, 0, 0)
        return header + struct.pack("<IIIIII", 0x32, 24, 1, 0, 0, 0)
    if "windows" in target:
        data = bytearray(64 + 24 + 112 + 40)
        data[:2] = b"MZ"
        struct.pack_into("<I", data, 60, 64)
        data[64:68] = b"PE\x00\x00"
        struct.pack_into("<HH", data, 68, 0xAA64 if arm else 0x8664, 1)
        struct.pack_into("<HHH", data, 84, 112, 2, 0x20B)
        struct.pack_into("<I", data, 104, 4096)
        struct.pack_into("<H", data, 156, 3)
        return bytes(data)
    data = bytearray(120)
    data[:7] = b"\x7fELF\x02\x01\x01"
    struct.pack_into("<HHIQQ", data, 16, 3, 183 if arm else 62, 1, 4096, 64)
    struct.pack_into("<HHH", data, 52, 64, 56, 1)
    struct.pack_into("<I", data, 64, 1)
    return bytes(data)


def metadata_fixture(metadata, **changes):
    """Copy file identity metadata and override only the property under test."""
    names = ("st_dev", "st_ino", "st_mode", "st_size", "st_mtime_ns", "st_ctime_ns",
             "st_birthtime_ns", "st_file_attributes")
    fields = {name: getattr(metadata, name) for name in names if hasattr(metadata, name)}
    fields.update(changes)
    return SimpleNamespace(**fields)


class FrigatePackageTests(unittest.TestCase):
    """Only a deliberately selected desktop executable becomes package payload."""

    def setUp(self):
        # TestCase owns cleanup even if an assertion fails.
        # pylint: disable-next=consider-using-with
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.worker = self.root / "worker"
        self.worker.write_bytes(executable_fixture("aarch64-apple-darwin"))
        self.output = self.root / "package"

    def test_stages_only_worker_and_unsigned_matching_metadata(self):
        """Nearby files and private signing material never enter the payload."""
        (self.root / "unrelated.key").write_bytes(os.urandom(32))
        packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        files = sorted(
            path.relative_to(self.output).as_posix()
            for path in self.output.rglob("*") if path.is_file()
        )
        self.assertEqual(files, ["manifest.json", "payload/bin/inverter-frigate-worker"])
        metadata = json.loads((self.output / "manifest.json").read_text())
        self.assertEqual(metadata["target"], "aarch64-apple-darwin")
        self.assertEqual(metadata["plugin_id"], "inverter-desktop.frigate")
        self.assertEqual(metadata["version"], "0.4.0")
        self.assertEqual(metadata["host_api"], "^1.8")
        self.assertEqual(metadata["http_video"], {
            "base_url_setting": "frigate_base_url",
            "live_preview": {"query": "fps=2&height=360", "max_duration_seconds": 15},
        })
        self.assertIn("http_video", metadata["permissions"])
        self.assertIsNone(metadata["signature"])
        self.assertEqual(metadata["inventory"], [])
        self.assertEqual(
            (self.output / "payload/bin/inverter-frigate-worker").read_bytes(),
            self.worker.read_bytes(),
        )
        fields = metadata["config_schema"]["properties"]
        self.assertTrue(fields["mqtt_username"]["writeOnly"])
        self.assertTrue(fields["mqtt_password"]["writeOnly"])
        self.assertEqual(fields["mqtt_topic"]["default"], "frigate/events")
        self.assertNotIn("frigate_base_url", metadata["config_schema"]["required"])
        self.assertEqual(fields["frigate_base_url"]["type"], "string")
        self.assertEqual(fields["frigate_base_url"]["maxLength"], 2048)
        excluded = fields["excluded_cameras"]
        self.assertEqual(excluded["default"], "[]")
        self.assertNotIn("writeOnly", excluded)
        self.assertEqual(excluded["maxLength"], 8192)
        self.assertEqual(excluded["x-editor"]["schema"], {
            "type": "array", "maxItems": 32,
            "items": {"type": "string", "title": "Camera ID",
                      "minLength": 1, "maxLength": 128},
        })

    def test_windows_entrypoint_uses_executable_suffix(self):
        """The same metadata template supports the Windows binary name."""
        self.worker.write_bytes(executable_fixture("x86_64-pc-windows-msvc"))
        packaging.prepare(self.worker, "x86_64-pc-windows-msvc", self.output)
        metadata = json.loads((self.output / "manifest.json").read_text())
        self.assertEqual(metadata["entrypoint"], "bin/inverter-frigate-worker.exe")
        self.assertTrue((self.output / "payload/bin/inverter-frigate-worker.exe").is_file())

    def test_refuses_mobile_targets_and_existing_output(self):
        """Mobile is outside the package ecosystem and existing work is preserved."""
        for target in ("aarch64-linux-android", "aarch64-apple-ios", "unrecognized"):
            with self.subTest(target=target), self.assertRaisesRegex(ValueError, "desktop"):
                packaging.prepare(self.worker, target, self.output)
        self.output.mkdir()
        keep = self.output / "keep"
        keep.write_text("unchanged")
        with self.assertRaises(FileExistsError):
            packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        self.assertEqual(keep.read_text(), "unchanged")

    def test_refuses_empty_directory_and_oversized_worker(self):
        """Input is bounded before allocating or creating staging files."""
        with self.assertRaisesRegex(ValueError, "regular"):
            packaging.prepare(self.root, "aarch64-apple-darwin", self.output)
        self.worker.write_bytes(b"")
        with self.assertRaisesRegex(ValueError, "size"):
            packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        with self.worker.open("wb") as stream:
            stream.truncate(packaging.MAX_WORKER_BYTES + 1)
        with self.assertRaisesRegex(ValueError, "size"):
            packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        self.assertFalse(self.output.exists())

    def test_every_supported_target_requires_matching_executable_headers(self):
        """OS and architecture mismatches cannot be hidden in manifest metadata."""
        for target in packaging.TARGETS:
            data = executable_fixture(target)
            packaging.verify_worker_target(data, target)
            for other in packaging.TARGETS:
                if target == other:
                    continue
                with (self.subTest(source=target, target=other),
                      self.assertRaisesRegex(ValueError, "match")):
                    packaging.verify_worker_target(data, other)

    def test_rejects_mobile_macho_dll_empty_and_truncated_headers(self):
        """Headers must identify a desktop executable, not merely a CPU family."""
        ios = bytearray(executable_fixture("aarch64-apple-darwin"))
        struct.pack_into("<I", ios, 40, 2)
        dll = bytearray(executable_fixture("x86_64-pc-windows-msvc"))
        struct.pack_into("<H", dll, 86, 0x2002)
        for target, data in (
            ("aarch64-apple-darwin", ios),
            ("x86_64-pc-windows-msvc", dll),
            ("aarch64-apple-darwin", b"not executable"),
            ("x86_64-unknown-linux-gnu", b"\x7fELF\x02\x01\x01"),
            ("aarch64-apple-darwin", b"\xcf\xfa\xed\xfe"),
        ):
            with self.subTest(target=target, data=data), self.assertRaises(ValueError):
                packaging.verify_worker_target(data, target)

    def test_macho_platform_commands_remain_strict(self):
        """Command bounds, duplicate platforms and legacy mobile tags are rejected."""
        target = "aarch64-apple-darwin"
        header = executable_fixture(target)[:32]
        modern = struct.pack("<IIIIII", 0x32, 24, 1, 0, 0, 0)
        legacy = struct.pack("<IIII", 0x24, 16, 0, 0)
        unrelated = struct.pack("<II", 0x1B, 8)

        def executable(commands):
            data = bytearray(header + b"".join(commands))
            struct.pack_into("<II", data, 16, len(commands), len(data) - 32)
            return data

        for commands in ((modern,), (legacy,), (unrelated, modern)):
            packaging.verify_worker_target(executable(commands), target)
        invalid = (
            (modern, modern), (modern, legacy), (unrelated,),
            (struct.pack("<II", 0x32, 8),),
            (struct.pack("<II", 0x24, 8),),
            (struct.pack("<II", 0x32, 0),),
            (struct.pack("<III", 0x32, 12, 1),),
            (struct.pack("<II", 0x32, 32),),
            (struct.pack("<IIII", 0x25, 16, 0, 0),),
            (struct.pack("<IIII", 0x2F, 16, 0, 0),),
            (struct.pack("<IIII", 0x30, 16, 0, 0),),
        )
        for commands in invalid:
            with self.subTest(commands=commands), self.assertRaises(ValueError):
                packaging.verify_worker_target(executable(commands), target)

    def test_architecture_failure_does_not_create_staging(self):
        """Mislabelled payloads are rejected before output files are created."""
        with (patch.object(Path, "mkdir", side_effect=AssertionError("Unexpected staging")),
              self.assertRaisesRegex(ValueError, "match")):
            packaging.prepare(self.worker, "x86_64-unknown-linux-gnu", self.output)
        self.assertFalse(self.output.exists())

    def test_rejects_file_replaced_after_path_inspection(self):
        """A replacement cannot become the fresh baseline for the open checks."""
        checked_path = packaging.checked_path

        def inspect_then_replace(path):
            result = checked_path(path)
            self.worker.rename(self.root / "original-worker")
            self.worker.write_bytes(executable_fixture("aarch64-apple-darwin"))
            return result

        with (patch.object(packaging, "checked_path", side_effect=inspect_then_replace),
              self.assertRaisesRegex(ValueError, "changed")):
            packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        self.assertFalse(self.output.exists())

    def test_rejects_parent_replaced_after_path_inspection(self):
        """Replacing an inspected directory must not select a different executable."""
        selected = self.root / "selected"
        selected.mkdir()
        worker = selected / "worker"
        worker.write_bytes(self.worker.read_bytes())
        checked_path = packaging.checked_path

        def inspect_then_replace(path):
            result = checked_path(path)
            selected.rename(self.root / "original-directory")
            selected.mkdir()
            worker.write_bytes(executable_fixture("aarch64-apple-darwin"))
            return result

        with (patch.object(packaging, "checked_path", side_effect=inspect_then_replace),
              self.assertRaisesRegex(ValueError, "changed")):
            packaging.prepare(worker, "aarch64-apple-darwin", self.output)
        self.assertFalse(self.output.exists())

    def test_rejects_file_replaced_between_validation_and_open(self):
        """The opened handle must match the file originally inspected."""
        open_file = os.open

        def replace_then_open(path, flags):
            self.worker.rename(self.root / "original-worker")
            self.worker.write_bytes(executable_fixture("aarch64-apple-darwin"))
            return open_file(path, flags)

        with (patch.object(os, "open", side_effect=replace_then_open),
              self.assertRaisesRegex(ValueError, "changed")):
            packaging.prepare(self.worker, "aarch64-apple-darwin", self.output)
        self.assertFalse(self.output.exists())

    @unittest.skipIf(os.name == "nt", "Creating symlinks requires Windows developer privileges")
    def test_rejects_canonical_escape_after_inspection(self):
        """Canonical containment detects a file redirected outside its selected parent."""
        selected = self.root / "selected"
        selected.mkdir()
        worker = selected / "worker"
        worker.write_bytes(self.worker.read_bytes())
        checked_path = packaging.checked_path

        def inspect_then_redirect(path):
            result = checked_path(path)
            worker.unlink()
            worker.symlink_to(self.worker)
            return result

        with (patch.object(packaging, "checked_path", side_effect=inspect_then_redirect),
              self.assertRaisesRegex(ValueError, "inside its inspected parent")):
            packaging.prepare(worker, "aarch64-apple-darwin", self.output)
        self.assertFalse(self.output.exists())

    @unittest.skipIf(os.name == "nt", "Creating symlinks requires Windows developer privileges")
    def test_refuses_links_in_file_and_parent_components(self):
        """Staging cannot silently follow a selected symlink or linked directory."""
        link = self.root / "link"
        link.symlink_to(self.worker)
        with self.assertRaisesRegex(ValueError, "symlinks"):
            packaging.prepare(link, "aarch64-apple-darwin", self.output)
        parent = self.root / "parent"
        parent.symlink_to(self.root, target_is_directory=True)
        source = parent / self.worker.name
        with self.assertRaisesRegex(ValueError, "symlinks"):
            packaging.prepare(source, "aarch64-apple-darwin", self.output)

    def test_cargo_hardlinked_input_is_copied_to_an_independent_file(self):
        """Cargo may hard-link its output; staging never preserves that linkage."""
        link = self.root / "hardlink"
        os.link(self.worker, link)
        packaging.prepare(link, "aarch64-apple-darwin", self.output)
        staged = self.output / "payload/bin/inverter-frigate-worker"
        self.assertEqual(staged.stat().st_nlink, 1)
        self.assertEqual(staged.read_bytes(), self.worker.read_bytes())
        self.assertFalse(os.path.samefile(staged, self.worker))

    def test_windows_creation_and_change_times_are_not_interchangeable(self):
        """The Python 3.12 path/fstat ctime difference does not reject unchanged input."""
        worker, inspected = packaging.checked_path(self.worker)
        source = metadata_fixture(inspected[-1][1], st_birthtime_ns=100, st_ctime_ns=100)
        inspected[-1] = (worker, source)
        opened = metadata_fixture(source, st_ctime_ns=200)
        with (patch.object(packaging, "IS_WINDOWS", True),
              patch.object(packaging, "checked_path", return_value=(worker, inspected)),
              patch.object(os, "fstat", side_effect=(opened, opened))):
            data = packaging.read_worker(self.worker)
        self.assertEqual(data, self.worker.read_bytes())
        with patch.object(packaging, "IS_WINDOWS", False):
            self.assertFalse(packaging.selected_file_matches(source, opened))

    def test_windows_path_to_handle_comparison_rejects_identity_and_content_changes(self):
        """Handling Windows ctime never relaxes inode, device, size or modification checks."""
        source = metadata_fixture(self.worker.stat(), st_birthtime_ns=100, st_ctime_ns=100)
        opened = metadata_fixture(source, st_ctime_ns=200)
        for field in ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_birthtime_ns"):
            changed = metadata_fixture(opened, **{field: getattr(opened, field) + 1})
            with self.subTest(field=field), patch.object(packaging, "IS_WINDOWS", True):
                self.assertFalse(packaging.selected_file_matches(source, changed))

    def test_older_windows_requires_matching_creation_time_and_rejects_mixed_fields(self):
        """Without birthtime, both legacy APIs use ctime as the same creation timestamp."""
        source = metadata_fixture(self.worker.stat(), st_ctime_ns=100)
        vars(source).pop("st_birthtime_ns", None)
        opened = metadata_fixture(source)
        changed = metadata_fixture(source, st_ctime_ns=200)
        with patch.object(packaging, "IS_WINDOWS", True):
            self.assertTrue(packaging.selected_file_matches(source, opened))
            self.assertFalse(packaging.selected_file_matches(source, changed))
            modern = metadata_fixture(source, st_birthtime_ns=100)
            self.assertFalse(packaging.selected_file_matches(source, modern))
            self.assertFalse(packaging.selected_file_matches(modern, source))

    def test_windows_read_rejects_same_inode_change_with_unchanged_size_and_mtime(self):
        """Full handle-to-handle ctime comparison detects changes while reading."""
        worker, inspected = packaging.checked_path(self.worker)
        source = metadata_fixture(inspected[-1][1], st_birthtime_ns=100, st_ctime_ns=100)
        inspected[-1] = (worker, source)
        opened = metadata_fixture(source, st_ctime_ns=200)
        changed = metadata_fixture(opened, st_ctime_ns=201)
        with (patch.object(packaging, "IS_WINDOWS", True),
              patch.object(packaging, "checked_path", return_value=(worker, inspected)),
              patch.object(os, "fstat", side_effect=(opened, changed)),
              self.assertRaisesRegex(ValueError, "changed")):
            packaging.read_worker(self.worker)

    def test_refuses_parent_traversal_before_normalizing_relative_paths(self):
        """Checking raw components prevents abspath from hiding traversal."""
        with self.assertRaisesRegex(ValueError, "traversal"):
            packaging.checked_path("unused/../worker")


if __name__ == "__main__":
    unittest.main()
