"""Exercise the production package dependency with the real, pinned Gradle.

The fixture replaces expensive native/Android tools with observable tasks, but
Gradle itself schedules both package tasks and their four shared ABI inputs.
No SDK, external Gradle plugins, signing keys or persisted build outputs are used.
"""

import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ANDROID = ROOT / "src-tauri/gen/android"
ABIS = ("Arm64", "Arm", "X86", "X86_64")
PROPERTY = "ORG_GRADLE_PROJECT_inverterAndroidBothPackages"


class AndroidPackageGraphTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.project = Path(self.temp.name)
        (self.project / "settings.gradle.kts").write_text(
            'rootProject.name = "package-graph"\n'
        )
        script = json.dumps(str(ANDROID / "app/release-packages.gradle.kts"))
        (self.project / "build.gradle.kts").write_text(
            f"apply(from = {script})\n"
            + """
val abis = listOf("Arm64", "Arm", "X86", "X86_64")
val rust = abis.map { abi ->
    tasks.register("rustBuild${abi}Release") {
        doLast {
            file("events").appendText("rust:$abi\\n")
            if (providers.gradleProperty("failAbi").orNull == abi) {
                throw GradleException("native failure: $abi")
            }
            file("$abi.so").writeText("current native input: $abi")
        }
    }
}
fun registerPackage(name: String, extension: String) = tasks.register(name) {
    dependsOn(rust)
    doLast {
        file("events").appendText("package:$extension\\n")
        if (providers.gradleProperty("failPackage").orNull == extension) {
            throw GradleException("package failure: $extension")
        }
        file("app.$extension").writeText(abis.joinToString("\\n") {
            file("$it.so").readText()
        })
    }
}
registerPackage("assembleUniversalRelease", "apk")
if (!providers.gradleProperty("omitBundle").isPresent) {
    registerPackage("bundleUniversalRelease", "aab")
}
tasks.register("assembleUniversalDebug") {
    doLast { file("events").appendText("debug\\n") }
}
"""
        )

    def run_gradle(self, *args, combined=True):
        env = os.environ.copy()
        env.pop(PROPERTY, None)
        if combined:
            env[PROPERTY] = "true"
        wrapper = ANDROID / ("gradlew.bat" if os.name == "nt" else "gradlew")
        return subprocess.run(
            [
                str(wrapper),
                "--project-dir",
                str(self.project),
                "--offline",
                "--console=plain",
                "--stacktrace",
                *args,
            ],
            env=env,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            timeout=180,
            check=False,
        )

    def events(self):
        path = self.project / "events"
        return path.read_text().splitlines() if path.exists() else []

    def assert_combined_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertCountEqual(
            self.events(),
            [*(f"rust:{abi}" for abi in ABIS), "package:apk", "package:aab"],
        )
        apk = (self.project / "app.apk").read_text()
        self.assertEqual(apk, (self.project / "app.aab").read_text())
        self.assertEqual(
            apk.splitlines(), [f"current native input: {abi}" for abi in ABIS]
        )

    def test_both_packages_share_all_four_native_tasks_and_rebuild_next_invocation(
        self,
    ):
        self.assert_combined_success(self.run_gradle("assembleUniversalRelease"))
        (self.project / "events").unlink()
        self.assert_combined_success(self.run_gradle("assembleUniversalRelease"))

    def test_apk_only_is_unchanged_without_opt_in(self):
        result = self.run_gradle("assembleUniversalRelease", combined=False)
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertCountEqual(
            self.events(), [*(f"rust:{abi}" for abi in ABIS), "package:apk"]
        )
        self.assertFalse((self.project / "app.aab").exists())

    def test_debug_does_not_acquire_release_tasks(self):
        result = self.run_gradle("assembleUniversalDebug")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertEqual(self.events(), ["debug"])

    def test_native_failure_stops_both_packages_on_cold_and_warm_builds(self):
        for warm in (False, True):
            with self.subTest(warm=warm):
                if warm:
                    (self.project / "events").unlink()
                    self.assert_combined_success(
                        self.run_gradle("assembleUniversalRelease")
                    )
                (self.project / "events").write_text("")
                result = self.run_gradle("assembleUniversalRelease", "-PfailAbi=Arm")
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn("native failure: Arm", result.stdout)
                self.assertFalse(
                    any(event.startswith("package:") for event in self.events())
                )

    def test_missing_bundle_task_fails_instead_of_silently_producing_only_apk(self):
        result = self.run_gradle("assembleUniversalRelease", "-PomitBundle=true")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("bundleUniversalRelease", result.stdout)
        self.assertEqual(self.events(), [])

    def test_failed_bundle_cannot_finish_apk_packaging(self):
        result = self.run_gradle("assembleUniversalRelease", "-PfailPackage=aab")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("package failure: aab", result.stdout)
        self.assertNotIn("package:apk", self.events())
        self.assertFalse((self.project / "app.apk").exists())


if __name__ == "__main__":
    unittest.main()
