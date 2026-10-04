#!/usr/bin/env python3
"""Compile the native acceptance harness once; isolate every scenario process."""

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

CASES = (
    "plugins::frigate_integration_tests::signed_frigate_package_real_mqtt_lifecycle",
    "plugins::frigate_integration_tests::signed_frigate_package_real_mqtt_live_lifecycle",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_lifecycle",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_actions",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_media",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_binary",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_cover",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_numeric",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_discovery",
    "plugins::home_assistant_integration_tests::signed_home_assistant_package_legacy_migration",
    "plugins::camera_integration_tests::signed_kerberos_package_real_mqtt_lifecycle",
    "plugins::camera_integration_tests::signed_kerberos_package_mqtt_url_preview_lifecycle",
    "plugins::camera_integration_tests::signed_ring_package_real_mqtt_snapshot_lifecycle",
)


def run_case(executable, directory, case):
    """Keep one scenario per process and require both a clean exit and one pass."""
    command = [str(executable), case, "--exact", "--ignored", "--test-threads=1",
               "--format=pretty", "--color=never"]
    passed = 0
    summaries = 0
    with subprocess.Popen(command, cwd=directory, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT) as process:
        for line in process.stdout:
            print(line, end="", flush=True)
            passed += line.strip() == f"test {case} ... ok"
            summaries += bool(re.fullmatch(
                r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; "
                r"\d+ filtered out; finished in [0-9.]+s", line.strip()))
        returncode = process.wait()
    if returncode:
        raise subprocess.CalledProcessError(returncode, command)
    if passed != 1 or summaries != 1:
        raise ValueError(f"Acceptance scenario did not run exactly once: {case}")


def validate_cases(cases):
    """Reject empty or ambiguous selections before building or running tests."""
    if not cases or len(set(cases)) != len(cases):
        raise ValueError("Acceptance scenarios must be nonempty and unique")
    if any(not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_:]*", case) for case in cases):
        raise ValueError("Invalid acceptance scenario name")


def run_harness(executable, manifest, cases, receipt):
    """Cargo supplies this freshly checked executable and its native runtime env."""
    validate_cases(cases)
    executable = executable.resolve()
    directory = manifest.resolve().parent
    if not executable.is_file() or not executable.is_relative_to(directory):
        raise ValueError("Library-test executable must exist inside its crate")
    inventory = subprocess.check_output(
        [str(executable), "--list", "--ignored", "--format=terse"],
        cwd=directory, text=True,
    )
    names = [line.removesuffix(": test") for line in inventory.splitlines()
             if line.endswith(": test")]
    if len(names) != len(cases) or set(names) != set(cases):
        raise ValueError("Ignored library-test inventory differs from acceptance scenarios")
    for case in cases:
        print(f"Acceptance scenario: {case}", flush=True)
        run_case(executable, directory, case)
    # A missing/misapplied runner must never turn into a successful default run.
    # Exclusive creation also rejects more than one harness invocation.
    with receipt.open("x") as stream:
        json.dump({"cases": cases, "executable": str(executable)}, stream)
    print(f"All {len(cases)} isolated acceptance scenarios passed.", flush=True)


def run_acceptance(root, cases=CASES):
    """Let Cargo build once and retain Cargo's cwd and dynamic-library environment."""
    validate_cases(cases)
    manifest = root.resolve() / "src-tauri/Cargo.toml"
    compiler = subprocess.check_output(["rustc", "-vV"], text=True)
    hosts = re.findall(r"^host: ([A-Za-z0-9_.-]+)$", compiler, re.MULTILINE)
    if len(hosts) != 1:
        raise ValueError("Cannot identify native Rust compiler host")
    with tempfile.TemporaryDirectory(prefix="ci-acceptance-receipt-") as temporary:
        receipt = Path(temporary) / "executed.json"
        runner = [sys.executable, str(Path(__file__).resolve()), "--harness",
                  str(manifest), json.dumps(cases), str(receipt)]
        # A command-local native runner does not add --target or change compiler flags.
        config = f"target.{hosts[0]}.runner={json.dumps(runner)}"
        subprocess.run(
            ["cargo", "test", "--locked", "--manifest-path", str(manifest), "--lib",
             "--config", config], cwd=manifest.parent, check=True,
        )
        if not receipt.is_file():
            raise ValueError("Cargo did not produce the acceptance execution receipt")
        proof = json.loads(receipt.read_text())
        if proof.get("cases") != list(cases):
            raise ValueError("Acceptance execution receipt has a different inventory")


def main():
    """Only the normal entry point compiles; Cargo invokes the internal adapter."""
    if len(sys.argv) == 1:
        run_acceptance(Path(__file__).resolve().parents[1])
    elif len(sys.argv) == 6 and sys.argv[1] == "--harness":
        manifest, cases, receipt, executable = sys.argv[2:]
        run_harness(Path(executable), Path(manifest), json.loads(cases), Path(receipt))
    else:
        raise ValueError("Unexpected acceptance runner arguments")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"Native acceptance failed: {error}")
