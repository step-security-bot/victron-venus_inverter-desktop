"""Release plugin orchestration with real staging and simulated compilation."""

import hashlib
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from unittest.mock import patch

from test_frigate_package import executable_fixture

CHECKOUT = Path(__file__).resolve().parents[1]
SCRIPTS = CHECKOUT / "scripts"
SPEC = importlib.util.spec_from_file_location(
    "release_plugins", SCRIPTS / "build-release-plugins.py")
release = importlib.util.module_from_spec(SPEC)
with patch.object(sys, "path", [str(SCRIPTS), *sys.path]):
    SPEC.loader.exec_module(release)


class ReleasePluginPackageTests(unittest.TestCase):
    """Never execute Cargo, workers or the native application in these fixtures."""

    def setUp(self):
        # TestCase cleanup covers all failed assertions and command simulations.
        # pylint: disable-next=consider-using-with
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.policy = json.loads((CHECKOUT / ".release-policy.json").read_text(encoding="utf-8"))
        self.plan = release.version_plan.create_plan(
            "2.5.42", "beta", 99, "a" * 40, self.policy,
            build_number=self.policy["versioning"].get("build_number_floor", 0) + 1)
        for plugin in release.plugin_package.PLUGINS:
            target = self.root / "desktop-plugins" / plugin / "Cargo.toml"
            target.parent.mkdir(parents=True)
            target.write_bytes((CHECKOUT / "desktop-plugins" / plugin / "Cargo.toml").read_bytes())
        self.commands = []

    def simulate_command(self, root, arguments):
        """Emit executable headers for staging and a stand-in archive for hashing."""
        self.assertEqual(root, self.root)
        args = list(map(str, arguments))
        self.commands.append(args)
        if args[0] == "cargo":
            self.assertIn("--locked", args)
            self.assertIn("--release", args)
            if "--bin" in args:
                binary = args[args.index("--bin") + 1]
                target = args[args.index("--target") + 1]
                worker = self.root / "src-tauri/target" / target / "release" / (
                    binary + (".exe" if "windows" in target else ""))
                worker.parent.mkdir(parents=True, exist_ok=True)
                worker.write_bytes(executable_fixture(target))
            return
        self.assertEqual(Path(args[0]).stem, "plugin-package")
        self.assertIn("--unsigned", args)
        self.assertNotIn("--key-id", args)
        self.assertNotIn("--key-file", args)
        manifest = Path(args[args.index("--manifest") + 1])
        output = Path(args[args.index("--output") + 1])
        with zipfile.ZipFile(output, "x") as archive:
            archive.writestr("manifest.json", manifest.read_bytes())

    def build(self, target="aarch64-apple-darwin", host="aarch64-apple-darwin"):
        """Exercise the complete orchestrator without invoking an executable."""
        with patch.object(release, "run", side_effect=self.simulate_command):
            return release.build_plugins(self.root, self.plan, "example/inverter", target, host)

    def test_every_desktop_target_produces_exact_pins_and_collision_free_names(self):
        """The platform artifacts can be flattened into one GitHub release."""
        names = set()
        for target in release.plugin_package.TARGETS:
            with self.subTest(target=target):
                paths = self.build(target, target)
                self.assertEqual(len(paths), 2 * len(release.plugin_package.PLUGINS) + 1)
                self.assertFalse(names.intersection(path.name for path in paths))
                names.update(path.name for path in paths)
                fragment = json.loads(paths[-1].read_text(encoding="utf-8"))
                self.assertEqual(len(fragment["desktop_plugins"]),
                                 len(release.plugin_package.PLUGINS))
                for declaration in fragment["desktop_plugins"]:
                    self.assertTrue(declaration["enabled"])
                    self.assertEqual(list(declaration["artifacts"]), [target])
                    artifact = declaration["artifacts"][target]
                    name = artifact["url"].rsplit("/", 1)[1]
                    package = paths[0].parent / name
                    digest = hashlib.sha256(package.read_bytes()).hexdigest()
                    self.assertEqual(artifact["sha256"], digest)
                    self.assertEqual(artifact["url"],
                                     f"https://github.com/example/inverter/releases/download/"
                                     f"{self.plan['tag']}/{name}")
                    self.assertEqual((package.parent / (name + ".sha256")).read_text(),
                                     f"{digest}  {name}\n")
                    with zipfile.ZipFile(package) as archive:
                        manifest = json.loads(archive.read("manifest.json"))
                    self.assertEqual(manifest["target"], target)
                    self.assertEqual(manifest["plugin_id"], declaration["plugin_id"])
                    self.assertEqual(manifest["version"], declaration["version"])
                    self.assertIsNone(manifest["signature"])

    def test_cross_compiled_workers_use_a_host_native_packager(self):
        """Intel macOS packages must not execute an Intel tool on an ARM builder."""
        self.build("x86_64-apple-darwin", "aarch64-apple-darwin")
        utility, *remaining = self.commands
        self.assertEqual(utility[utility.index("--target") + 1], "aarch64-apple-darwin")
        for command in remaining:
            if command[0] == "cargo":
                self.assertEqual(command[command.index("--target") + 1], "x86_64-apple-darwin")
            else:
                self.assertEqual(
                    Path(command[0]), self.root / "src-tauri" / "target" /
                    "aarch64-apple-darwin" / "release" / "examples" / "plugin-package")

    def test_existing_outputs_are_not_replaced(self):
        """A retried build cannot silently replace already collected release bytes."""
        paths = self.build()
        before = {path: path.read_bytes() for path in paths}
        with self.assertRaisesRegex(ValueError, "already exists"):
            self.build()
        self.assertEqual({path: path.read_bytes() for path in paths}, before)
        self.assertEqual(list((self.root / "release-output").iterdir()), [paths[0].parent])

    def test_compile_failure_does_not_publish_a_partial_plugin_set(self):
        """Failed compilation leaves no apparently publishable plugin assets."""
        with patch.object(release, "run", side_effect=subprocess.CalledProcessError(1, "cargo")):
            with self.assertRaises(subprocess.CalledProcessError):
                release.build_plugins(self.root, self.plan, "example/inverter",
                                      "aarch64-apple-darwin", "aarch64-apple-darwin")
        self.assertEqual(list((self.root / "release-output/desktop").iterdir()), [])

    def test_worker_version_drift_is_rejected_before_publication(self):
        """Metadata must identify the actual independently versioned worker."""
        cargo = self.root / "desktop-plugins/frigate/Cargo.toml"
        cargo.write_text(cargo.read_text().replace('version = "0.4.1"', 'version = "99.0.0"'))
        with self.assertRaisesRegex(ValueError, "version does not match"):
            self.build()
        self.assertEqual(list((self.root / "release-output/desktop").iterdir()), [])

    def test_target_and_repository_validation_precedes_build_commands(self):
        """Mobile ABIs and URL injection are rejected before starting Cargo."""
        cases = [
                ("aarch64-linux-android", "aarch64-apple-darwin", "example/inverter"),
                ("aarch64-apple-darwin", "aarch64-apple-ios", "example/inverter"),
                ("aarch64-apple-darwin", "aarch64-apple-darwin", "example/inverter?token=x")]
        for invalid in ["--config=malicious.toml", "../aarch64-apple-darwin",
                        "/tmp/target.json", "aarch64-apple-darwin\n--config=malicious.toml"]:
            cases.extend([(invalid, "aarch64-apple-darwin", "example/inverter"),
                          ("aarch64-apple-darwin", invalid, "example/inverter")])
        for target, host, repository in cases:
            with self.subTest(target=target, host=host, repository=repository):
                with patch.object(release, "run") as command:
                    with self.assertRaises(ValueError):
                        release.build_plugins(self.root, self.plan, repository, target, host)
                    command.assert_not_called()

    def test_commands_receive_canonical_target_constants(self):
        """Accepted caller strings never become command arguments themselves."""
        class TargetText(str):
            """An equal string with an unsafe conversion must not reach a command."""

            def __str__(self):
                return "--config=malicious.toml"

        for target in release.plugin_package.TARGETS:
            selected = release.desktop_target(TargetText(target))
            self.assertIs(selected, target)
            self.assertIs(type(selected), str)
        self.build(TargetText("x86_64-apple-darwin"), TargetText("aarch64-apple-darwin"))
        for command in self.commands:
            self.assertFalse(any("malicious" in argument for argument in command))

    def test_frozen_plan_must_match_checkout_and_policy(self):
        """Download URLs cannot point at a release plan from another source SHA."""
        (self.root / ".release-plan.json").write_text(json.dumps(self.plan))
        (self.root / ".release-policy.json").write_text(json.dumps(self.policy))
        with patch.object(release.subprocess, "check_output", return_value="a" * 40 + "\n"):
            self.assertEqual(release.load_release_plan(self.root), self.plan)
        with patch.object(release.subprocess, "check_output", return_value="b" * 40 + "\n"):
            with self.assertRaises(ValueError):
                release.load_release_plan(self.root)

    def test_compiler_host_is_independent_of_cross_compile_environment(self):
        """The worker target override does not choose the packaging tool ABI."""
        with patch.object(release.subprocess, "check_output", return_value=(
                "rustc 1.99.0\nhost: aarch64-apple-darwin\nrelease: 1.99.0\n")):
            with patch.dict(release.os.environ, {"TAURI_TARGET": "x86_64-apple-darwin"}):
                self.assertEqual(release.compiler_host(self.root), "aarch64-apple-darwin")


if __name__ == "__main__":
    unittest.main()
