#!/usr/bin/env python3
"""Build pinned desktop plugin assets for the already frozen release identity.

Workers are compiled but never executed. The host-native packaging utility needs
no signing keys. Its packages, checksums and portable config fragment join the
desktop installers before the existing release input receipt is created.
"""

# CLI filenames use hyphens to match their shell entry points.
# pylint: disable=invalid-name

import argparse
import hashlib
import json
import os
import re
import subprocess
import tempfile
import tomllib
from pathlib import Path
from urllib.parse import quote

import version_plan
from plugins import plugin_package

ROOT = Path(__file__).resolve().parents[1]


def desktop_target(value):
    """Select a trusted ABI constant instead of forwarding caller-supplied text."""
    for target in plugin_package.TARGETS:
        if value == target:
            return target
    raise ValueError("Release plugins require a supported desktop target")


def compiler_host(root):
    """Read the actual host, independent of a cross-compilation target override."""
    output = subprocess.check_output(["rustc", "-vV"], cwd=root, text=True)
    hosts = [line.removeprefix("host: ") for line in output.splitlines()
             if line.startswith("host: ")]
    if len(hosts) != 1:
        raise ValueError("Cannot identify the Rust compiler host")
    return desktop_target(hosts[0])


def repository_name(value):
    """Only a literal GitHub owner/repository may supply release download URLs."""
    if not isinstance(value, str) or not re.fullmatch(
            r"[A-Za-z0-9][A-Za-z0-9_.-]{0,99}/[A-Za-z0-9][A-Za-z0-9_.-]{0,99}", value):
        raise ValueError("Expected a GitHub owner/repository")
    return value


def load_release_plan(root):
    """Bind plugin output URLs to the same checked source and policy as installers."""
    plan = json.loads((root / ".release-plan.json").read_text(encoding="utf-8"))
    policy = json.loads((root / ".release-policy.json").read_text(encoding="utf-8"))
    source = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    return version_plan.validate_plan(plan, policy=policy, source_sha=source)


def run(root, arguments):
    """Run one checked build/packaging command without invoking any worker."""
    subprocess.run([str(argument) for argument in arguments], cwd=root, check=True)


def encode_json(value):
    """Write stable portable config bytes for receipt hashing."""
    return (json.dumps(value, indent=2, sort_keys=True) + "\n").encode("utf-8")


# Each target produces the registered workers and one host-native packaging utility.
# pylint: disable=too-many-locals
def build_plugins(root, plan, repository, target, host, *, implicit_native=False):
    """Create target-unique release assets without overwriting an existing artifact."""
    target = desktop_target(target)
    host = desktop_target(host)
    if implicit_native and target != host:
        raise ValueError("Implicit native builds require the compiler host target")
    repository_name(repository)
    version_plan.validate_plan(plan)
    output = root / "release-output" / "desktop"
    output.mkdir(parents=True, exist_ok=True)
    plugin_package.checked_path(output)
    target_dir = root / "src-tauri" / "target"
    # Windows/Linux installers use Cargo's native layout. Reuse their dependency
    # artifacts instead of rebuilding the same crate graph under <host>/release.
    host_args = [] if implicit_native else ["--target", host]
    worker_args = [] if implicit_native else ["--target", target]
    run(root, ["cargo", "build", "--locked", "--release", "--manifest-path",
               root / "src-tauri/Cargo.toml", "--example", "plugin-package",
               *host_args, "--target-dir", target_dir])
    host_directory = target_dir if implicit_native else target_dir / host
    worker_directory = target_dir if implicit_native else target_dir / target
    packager = host_directory / "release/examples" / (
        "plugin-package.exe" if "windows" in host else "plugin-package")
    declarations = []
    published = []
    with tempfile.TemporaryDirectory(prefix="plugin-build-", dir=output.parent) as temporary:
        staging = Path(temporary).resolve()
        for plugin, (_, binary) in plugin_package.PLUGINS.items():
            worker_manifest = root / "desktop-plugins" / plugin / "Cargo.toml"
            package = tomllib.loads(worker_manifest.read_text(encoding="utf-8"))["package"]
            if package["name"] != binary:
                raise ValueError("Worker Cargo identity does not match package metadata")
            run(root, ["cargo", "build", "--locked", "--release", "--manifest-path",
                       worker_manifest, "--bin", binary, *worker_args,
                       "--target-dir", target_dir])
            worker = worker_directory / "release" / (
                binary + (".exe" if "windows" in target else ""))
            prepared = plugin_package.prepare(worker, target, staging / plugin, plugin)
            metadata = json.loads((prepared / "manifest.json").read_text(encoding="utf-8"))
            if metadata["version"] != package["version"]:
                raise ValueError("Worker Cargo version does not match package metadata")
            name = f"{metadata['plugin_id']}-{metadata['version']}-{target}.idplugin"
            archive = staging / name
            run(root, [packager, "--unsigned", "--manifest", prepared / "manifest.json",
                       "--root", prepared / "payload", "--output", archive])
            data = archive.read_bytes()
            if not data:
                raise ValueError("Packaging utility produced an empty archive")
            digest = hashlib.sha256(data).hexdigest()
            published.extend([(name, data), (name + ".sha256", f"{digest}  {name}\n".encode())])
            url = (f"https://github.com/{repository}/releases/download/"
                   f"{quote(plan['tag'], safe='')}/{quote(name, safe='')}")
            declarations.append({
                "plugin_id": metadata["plugin_id"], "version": metadata["version"],
                "enabled": True, "artifacts": {target: {"url": url, "sha256": digest}},
            })
        published.append((f"desktop-plugins-{target}.json",
                          encode_json({"desktop_plugins": declarations})))
        if any((output / name).exists() or (output / name).is_symlink() for name, _ in published):
            raise ValueError("Release plugin output already exists")
        for name, data in published:
            with (output / name).open("xb") as stream:
                stream.write(data)
    return [output / name for name, _ in published]


def main():
    """Package this runner's desktop target using its frozen GitHub release plan."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", default=os.environ.get("TAURI_TARGET") or None)
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"))
    args = parser.parse_args()
    try:
        plan = load_release_plan(ROOT)
        repository = repository_name(args.repository)
        host = compiler_host(ROOT)
        paths = build_plugins(ROOT, plan, repository, args.target or host, host,
                              implicit_native=args.target is None)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Cannot build release plugins: {error}\n")
    for path in paths:
        print(path.name)


if __name__ == "__main__":
    main()
