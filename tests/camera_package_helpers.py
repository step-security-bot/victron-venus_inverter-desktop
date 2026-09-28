"""Shared assertions for independent camera package staging; no worker execution."""

import json
import tempfile
import tomllib
from pathlib import Path

from test_frigate_package import executable_fixture, packaging


def validate_camera_staging(test, plugin):
    """Check native headers, identity and exact payload layout on all six targets."""
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary).resolve()
        worker = root / "worker"
        for target in packaging.TARGETS:
            with test.subTest(target=target):
                worker.write_bytes(executable_fixture(target))
                output = root / target
                packaging.prepare(worker, target, output, plugin)
                manifest = json.loads((output / "manifest.json").read_text())
                binary = f"inverter-{plugin}-worker"
                if "windows" in target:
                    binary += ".exe"
                test.assertEqual(manifest["plugin_id"], f"inverter-desktop.{plugin}")
                test.assertEqual(manifest["version"], "0.2.0" if plugin == "kerberos" else "0.1.0")
                test.assertEqual(manifest["host_api"], "^1.9" if plugin == "kerberos" else "^1.8")
                test.assertEqual(manifest["target"], target)
                test.assertEqual(manifest["entrypoint"], f"bin/{binary}")
                test.assertEqual(manifest["group"], {
                    "id": "cameras", "title": "Cameras", "icon": "camera",
                })
                test.assertIsNone(manifest["signature"])
                test.assertEqual(manifest["inventory"], [])
                test.assertEqual(sorted(path.relative_to(output).as_posix()
                                        for path in output.rglob("*") if path.is_file()),
                                 ["manifest.json", f"payload/bin/{binary}"])
                test.assertEqual((output / "payload/bin" / binary).read_bytes(),
                                 worker.read_bytes())
        checkout = Path(__file__).resolve().parents[1]
        cargo = tomllib.loads((checkout / "desktop-plugins" / plugin / "Cargo.toml").read_text())
        test.assertEqual(cargo["package"]["version"], manifest["version"])
        return manifest
