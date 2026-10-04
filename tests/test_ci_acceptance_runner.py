"""Exercise the CI runner against real Cargo and failing native test processes."""

import contextlib
import importlib.util
import io
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("ci_acceptance", ROOT / "scripts/run-ci-acceptance.py")
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("Cannot load CI acceptance runner")
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)


class AcceptanceRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="acceptance-runner-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.crate = self.root / "src-tauri"
        (self.crate / "src").mkdir(parents=True)
        (self.crate / "Cargo.toml").write_text(
            '[package]\nname="acceptance-probe"\nversion="0.1.0"\nedition="2021"\n')
        # Reproduce the real application's absent optional release-plan watcher.
        (self.crate / "build.rs").write_text('''
use std::{fs::OpenOptions, io::Write};
fn main() {
    println!("cargo:rerun-if-changed=.release-plan.json");
    writeln!(OpenOptions::new().create(true).append(true).open("builds").unwrap(), "build").unwrap();
}
''')
        self.source = '''
fn record() {
    use std::{fs::OpenOptions, io::Write};
    assert_eq!(std::env::var("CARGO_MANIFEST_DIR").unwrap(), env!("CARGO_MANIFEST_DIR"));
    writeln!(OpenOptions::new().create(true).append(true).open("runs").unwrap(), "{}", std::process::id()).unwrap();
}
#[test] #[ignore] fn first() { record(); }
#[test] #[ignore] fn second() { record(); }
'''
        (self.crate / "src/lib.rs").write_text(self.source)
        subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=self.crate,
                       check=True, capture_output=True)
        self.environment = patch.dict(os.environ, {"CARGO_NET_OFFLINE": "true"})
        self.environment.start()
        self.addCleanup(self.environment.stop)

    def run_cases(self, cases=("first", "second")):
        with contextlib.redirect_stdout(io.StringIO()):
            RUNNER.run_acceptance(self.root, cases)

    def test_build_once_and_keep_distinct_processes_on_each_invocation(self):
        self.run_cases()
        self.assertEqual((self.crate / "builds").read_text().splitlines(), ["build"])
        processes = (self.crate / "runs").read_text().splitlines()
        self.assertEqual(len(processes), 2)
        self.assertEqual(len(set(processes)), 2)
        self.run_cases()
        self.assertEqual((self.crate / "builds").read_text().splitlines(), ["build", "build"])
        self.assertEqual(len((self.crate / "runs").read_text().splitlines()), 4)

    def test_missing_or_extra_scenarios_fail_before_running_any(self):
        for cases in (("first", "missing"), ("first",)):
            with self.subTest(cases=cases), self.assertRaises(subprocess.CalledProcessError):
                self.run_cases(cases)
        self.assertFalse((self.crate / "runs").exists())

    def test_real_failure_stops_before_the_following_scenario(self):
        (self.crate / "src/lib.rs").write_text(
            self.source.replace("fn first() { record(); }", 'fn first() { record(); panic!("failure"); }'))
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_cases()
        self.assertEqual(len((self.crate / "runs").read_text().splitlines()), 1)

    def test_compile_failure_cannot_reuse_a_previously_built_harness(self):
        self.run_cases()
        (self.crate / "src/lib.rs").write_text("this does not compile")
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_cases()
        self.assertEqual(len((self.crate / "runs").read_text().splitlines()), 2)

    def test_duplicate_names_fail_before_build(self):
        with self.assertRaisesRegex(ValueError, "unique"):
            self.run_cases(("first", "first"))
        self.assertFalse((self.crate / "builds").exists())

    def test_zero_tests_and_nonzero_exit_after_success_text_are_rejected(self):
        fake = self.crate / "fake-test"
        for status, passed in ((0, 0), (1, 1)):
            text = (f"test first ... ok\ntest result: ok. {passed} passed; 0 failed; "
                    "0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n")
            fake.write_text(f"#!{sys.executable}\nimport sys\nprint({text!r})\nsys.exit({status})\n")
            fake.chmod(0o700)
            with (self.subTest(status=status), contextlib.redirect_stdout(io.StringIO()),
                  self.assertRaises((ValueError, subprocess.CalledProcessError))):
                RUNNER.run_case(fake, self.crate, "first")

    def test_cargo_success_without_executing_the_adapter_is_rejected(self):
        original = subprocess.run

        def omit_runner(arguments, **kwargs):
            if arguments[0] == "cargo":
                return subprocess.CompletedProcess(arguments, 0)
            return original(arguments, **kwargs)

        with (patch.object(RUNNER.subprocess, "run", side_effect=omit_runner),
              self.assertRaisesRegex(ValueError, "execution receipt")):
            self.run_cases()


if __name__ == "__main__":
    unittest.main()
