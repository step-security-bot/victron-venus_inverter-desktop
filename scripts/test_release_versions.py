"""Exercise this application's version projections and packaged metadata checks."""

import importlib.util
import hashlib
import json
import os
from pathlib import Path
import plistlib
import platform
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from contextlib import chdir
from unittest.mock import patch
import zipfile

from ios_version import sync_plist, verify_ipa
from version_plan import (
    check_base_versions,
    create_plan,
    projected_value,
    sync_versions,
)
from version_receipt import verify_current_inputs

ROOT = Path(__file__).resolve().parents[1]


def script(name):
    """Load a hyphenated CLI entry point for direct contract checks."""
    spec = importlib.util.spec_from_file_location(name, ROOT / "scripts" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ReleaseVersionTests(unittest.TestCase):
    """Verify application projections and the metadata inside real packages."""

    def setUp(self):
        """Copy the declared version files into an isolated fixture checkout."""
        # unittest owns cleanup across the full test, including assertion failures.
        # pylint: disable-next=consider-using-with
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.policy = json.loads((ROOT / ".release-policy.json").read_bytes())
        self.base = json.loads((ROOT / "package.json").read_bytes())["version"]
        self.build = self.policy["versioning"]["build_number_floor"] + 1
        for name in {item["path"] for item in self.policy["versioning"]["files"]}:
            target = self.root / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(ROOT / name, target)
        (self.root / ".release-policy.json").write_text(json.dumps(self.policy))
        self.sha = "a" * 40

    def plan(self, channel="beta", sequence=3, number=None):
        """Create a candidate above this application's migration counter floor."""
        return create_plan(
            self.base, channel, sequence, self.sha, self.policy, number or self.build
        )

    def test_all_owned_manifests_and_lock_match_candidate_but_native_base_is_numeric(
        self,
    ):
        """Keep full candidate identity while projecting native numeric fields."""
        check_base_versions(self.root, self.policy)
        plan = self.plan()
        sync_versions(self.root, self.policy, plan)
        sync_versions(self.root, self.policy, plan, check=True)
        config = json.loads((self.root / "src-tauri/tauri.conf.json").read_bytes())
        self.assertEqual(config["version"], self.base)
        self.assertEqual(
            config["bundle"]["iOS"]["bundleVersion"],
            projected_value(plan, {"value": "apple-build"}),
        )
        self.assertEqual(
            json.loads((self.root / "package.json").read_bytes())["version"],
            plan["version"],
        )
        self.assertIn(
            f'version = "{self.base}-beta.3"',
            (self.root / "src-tauri/Cargo.lock").read_text(),
        )

    def test_stable_requires_a_plan_matching_source_and_channel(self):
        """Reject absent or stale stable plans and accept the exact planned source."""
        checker = script("check-release-version")
        with self.assertRaisesRegex(ValueError, "verified"):
            checker.check_release_version(self.root, self.base, "stable")
        path = self.root / ".release-plan.json"
        path.write_text(json.dumps(self.plan("stable", None)))
        with patch.object(checker.subprocess, "check_output", return_value=self.sha):
            checker.check_release_version(self.root, self.base, "stable")
        with patch.object(checker.subprocess, "check_output", return_value="b" * 40):
            with self.assertRaises(ValueError):
                checker.check_release_version(self.root, self.base, "stable")

    def test_windows_checkout_preserves_receipted_cargo_inputs(self):
        """Keep Cargo input hashes stable through Windows checkout and Tauri LF writes."""
        shutil.copy2(ROOT / ".gitattributes", self.root / ".gitattributes")
        commands = (
            ["git", "init", "-q"],
            ["git", "config", "core.autocrlf", "true"],
            ["git", "add", "."],
        )
        for command in commands:
            subprocess.run(command, cwd=self.root, check=True, capture_output=True)
        cargo_paths = ("src-tauri/Cargo.toml", "src-tauri/Cargo.lock")
        for name in cargo_paths:
            (self.root / name).unlink()
        subprocess.run(
            ["git", "checkout-index", "--all", "--force"],
            cwd=self.root,
            check=True,
            capture_output=True,
        )
        for name in cargo_paths:
            self.assertNotIn(b"\r\n", (self.root / name).read_bytes(), name)

        evidence = sync_versions(self.root, self.policy, self.plan())
        for name in cargo_paths:
            path = self.root / name
            # Tauri serializes Cargo.toml to LF even on Windows. Match that
            # boundary here without compiling a platform application in a unit test.
            path.write_bytes(path.read_bytes().replace(b"\r\n", b"\n"))
        verify_current_inputs(self.root, evidence)

        manifest = self.root / "src-tauri/Cargo.toml"
        manifest.write_bytes(manifest.read_bytes() + b"# unrelated build mutation\n")
        with self.assertRaisesRegex(ValueError, "Build input changed after version sync"):
            verify_current_inputs(self.root, evidence)

    def test_tauri_asset_protocol_is_declared_for_every_platform_dependency(self):
        """Prevent Tauri init/build from rewriting a receipted Cargo manifest."""
        config = json.loads((self.root / "src-tauri/tauri.conf.json").read_bytes())
        manifest = tomllib.loads((self.root / "src-tauri/Cargo.toml").read_text())
        if not config["app"]["security"]["assetProtocol"]["enable"]:
            return
        dependencies = {"dependencies": manifest["dependencies"]}
        dependencies.update(
            {
                f"target.{target}.dependencies": settings.get("dependencies", {})
                for target, settings in manifest.get("target", {}).items()
            }
        )
        for table, entries in dependencies.items():
            if "tauri" not in entries:
                continue
            with self.subTest(table=table):
                self.assertIsInstance(entries["tauri"], dict)
                self.assertIn(
                    "protocol-asset",
                    entries["tauri"].get("features", []),
                    "Tauri injects configured asset protocol into every dependency "
                    "table, including targets inactive on the current platform",
                )

    def test_ios_package_keeps_numeric_versions_and_full_candidate_identity(self):
        """Inspect the plist and embedded identity in an actual IPA archive."""
        plan = self.plan()
        build = projected_value(plan, {"value": "apple-build"})
        plist = self.root / "Info.plist"
        plist.write_bytes(plistlib.dumps({"CFBundleExecutable": "App"}))
        sync_plist(plist, plan["base_version"], build, plan["version"])
        ipa = self.root / "App.ipa"
        with zipfile.ZipFile(ipa, "w") as archive:
            archive.write(plist, "Payload/App.app/Info.plist")
            archive.writestr(
                "Payload/App.app/App", json.dumps(plan, separators=(",", ":"))
            )
        verify_ipa(ipa, plan["base_version"], build, plan["version"])
        verifier = script("verify-native-release")
        verifier.verify_apple(ipa, plan)
        with self.assertRaises(ValueError):
            verifier.verify_apple(ipa, self.plan("rc", 1, self.build + 1))
        with self.assertRaises(ValueError):
            verifier.verify_embedded_identity(b"old binary", plan)

    def test_native_cli_rejects_escape_and_symlink_before_inspection(self):
        """Constrain caller-selected packages to regular paths in this checkout."""
        verifier = script("verify-native-release")
        plan_path = self.root / ".release-plan.json"
        plan_path.write_text(json.dumps(self.plan()))
        artifact = self.root / "package.deb"
        artifact.write_bytes(b"fixture")
        alias = self.root / "alias.deb"
        alias.symlink_to(artifact)
        with tempfile.TemporaryDirectory() as temp:
            outside = Path(temp) / "outside.deb"
            outside.write_bytes(b"outside checkout")
            for path in (outside, alias, self.root / "nested/../package.deb"):
                with (
                    self.subTest(path=path),
                    patch.object(verifier, "CHECKOUT", self.root),
                    patch.object(
                        sys,
                        "argv",
                        [
                            "verify-native-release",
                            "linux",
                            str(path),
                            "--plan",
                            str(plan_path),
                        ],
                    ),
                    patch.object(verifier, "verify_linux") as inspect,
                    self.assertRaises(ValueError),
                ):
                    verifier.main()
                inspect.assert_not_called()

    def test_native_cli_resolves_option_like_filenames_before_inspection(self):
        """An option-shaped filename must reach tools as an absolute file path."""
        verifier = script("verify-native-release")
        plan_path = self.root / ".release-plan.json"
        plan_path.write_text(json.dumps(self.plan()))
        artifact = self.root / "--queryformat=.deb"
        artifact.write_bytes(b"fixture")
        with (
            chdir(self.root),
            patch.object(verifier, "CHECKOUT", self.root),
            patch.object(
                sys,
                "argv",
                [
                    "verify-native-release",
                    "linux",
                    "--plan",
                    str(plan_path),
                    "--",
                    artifact.name,
                ],
            ),
            patch.object(verifier, "verify_linux") as inspect,
        ):
            verifier.main()
        self.assertEqual(inspect.call_args.args[0], artifact.resolve())

    def test_apk_inspector_only_runs_the_expected_sdk_binary(self):
        """Reject arbitrary executables before inspecting genuine APK ZIP metadata."""
        verifier = script("verify-native-release")
        plan = self.plan()
        apk = self.root / "package.apk"
        with zipfile.ZipFile(apk, "w") as archive:
            archive.writestr(
                "lib/arm64-v8a/libinverter_dashboard_lib.so",
                json.dumps(plan, separators=(",", ":")),
            )
        sdk = self.root / "sdk"
        aapt = sdk / "build-tools/34.0.0/aapt"
        aapt.parent.mkdir(parents=True)
        aapt.write_bytes(b"fixture inspector")
        other = sdk / "other-aapt"
        other.write_bytes(b"caller-selected executable")
        output = (
            "package: name='com.alvit.inverter_dashboard' "
            f"versionCode='{plan['build_number']}' versionName='{plan['version']}'"
        )
        with (
            patch.dict(os.environ, {"ANDROID_HOME": str(sdk)}),
            patch.object(
                verifier.subprocess, "check_output", return_value=output
            ) as execute,
        ):
            with self.assertRaisesRegex(ValueError, "Inspector must be"):
                verifier.verify_apk(apk, plan, other)
            execute.assert_not_called()
            verifier.verify_apk(apk, plan, aapt)
        self.assertEqual(execute.call_args.args[0][0], str(aapt.resolve()))
        self.assertEqual(execute.call_args.args[0][-1], str(apk.resolve()))

    def test_aab_inspector_requires_fixed_path_and_matching_checksum(self):
        """Validate the selected bundletool bytes before any Java invocation."""
        verifier = script("verify-native-release")
        plan = self.plan()
        aab = self.root / "package.aab"
        with zipfile.ZipFile(aab, "w") as archive:
            archive.writestr(
                "base/lib/arm64-v8a/libinverter_dashboard_lib.so",
                json.dumps(plan, separators=(",", ":")),
            )
        runner = self.root / "runner"
        runner.mkdir()
        bundletool = runner / "bundletool.jar"
        original = b"trusted fixture inspector"
        bundletool.write_bytes(original)
        other = runner / "other.jar"
        other.write_bytes(original)
        manifest = (
            '<manifest xmlns:android="http://schemas.android.com/apk/res/android" '
            f'package="com.alvit.inverter_dashboard" android:versionName="{plan["version"]}" '
            f'android:versionCode="{plan["build_number"]}" />'
        )
        with (
            patch.dict(os.environ, {"RUNNER_TEMP": str(runner)}),
            patch.object(
                verifier, "BUNDLETOOL_SHA256", hashlib.sha256(original).hexdigest()
            ),
            patch.object(
                verifier.subprocess, "check_output", return_value=manifest
            ) as execute,
        ):
            with self.assertRaisesRegex(ValueError, "Inspector must be"):
                verifier.verify_aab(aab, plan, other)
            bundletool.write_bytes(b"modified inspector")
            with self.assertRaisesRegex(ValueError, "checksum"):
                verifier.verify_aab(aab, plan, bundletool)
            execute.assert_not_called()
            bundletool.write_bytes(original)
            verifier.verify_aab(aab, plan, bundletool)
        self.assertEqual(execute.call_args.args[0][2], str(bundletool.resolve()))

    @unittest.skipUnless(shutil.which("dpkg-deb"), "dpkg-deb is not installed")
    def test_real_debian_archive_rejects_stale_package_metadata(self):
        """Reject an actual Debian archive after its native version becomes stale."""
        plan = self.plan()
        package = self.root / "deb"
        (package / "DEBIAN").mkdir(parents=True)
        (package / "usr/bin").mkdir(parents=True)
        control = package / "DEBIAN/control"
        architecture = "amd64" if platform.machine() == "x86_64" else "arm64"
        control.write_text(
            f"Package: inverter-dashboard\nVersion: {self.base}\nArchitecture: {architecture}\n"
            "Maintainer: Inverter Desktop\nDescription: Version validation fixture\n"
        )
        (package / "usr/bin/inverter-dashboard").write_text(
            json.dumps(plan, separators=(",", ":"))
        )
        artifact = self.root / "fixture.deb"
        subprocess.run(
            ["dpkg-deb", "--build", str(package), str(artifact)],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        verifier = script("verify-native-release")
        verifier.verify_linux(artifact, plan)
        control.write_text(
            control.read_text().replace(
                f"Version: {self.base}",
                "Version: 0.0.0" if self.base != "0.0.0" else "Version: 0.0.1",
            )
        )
        subprocess.run(
            ["dpkg-deb", "--build", str(package), str(artifact)],
            check=True,
            stdout=subprocess.DEVNULL,
        )
        with self.assertRaisesRegex(ValueError, "Linux package version"):
            verifier.verify_linux(artifact, plan)


if __name__ == "__main__":
    unittest.main()
