#!/usr/bin/env python3
"""Compile all desktop workers and create local .idplugin artifacts with checksums.

No release plan, network publication or signing key is needed. Artifacts are
published to a new local directory only when every worker packages successfully.
Use --install on macOS to update already installed plugins in the desktop app.
Release pins and encrypted settings are preserved; --restore-release undoes the
local selection. The app must include local-build support and be unlocked.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from pathlib import Path

from macos_app import APP, restart_app
from plugins import plugin_package

ROOT = Path(__file__).resolve().parents[1]
SELECTION = 'local-plugin-overrides.json'


def compiler_host(root):
    """Restrict native builds to a supported desktop ABI."""
    output = subprocess.check_output(["rustc", "-vV"], cwd=root, text=True)
    hosts = [line.removeprefix("host: ") for line in output.splitlines()
             if line.startswith("host: ")]
    if len(hosts) != 1 or hosts[0] not in plugin_package.TARGETS:
        raise ValueError("Local plugins require a supported desktop Rust host")
    return hosts[0]


def run(root, arguments):
    """Compile or package without executing a worker."""
    subprocess.run([str(argument) for argument in arguments], cwd=root, check=True, stdout=sys.stderr)


def build_plugins(root, target):
    """Build one complete native plugin set, retaining previous successful sets."""
    if target not in plugin_package.TARGETS:
        raise ValueError("Local plugins require a supported desktop target")
    target_dir = root / "src-tauri" / "target"
    output = root / "target" / "local-plugins" / target
    output.mkdir(parents=True, exist_ok=True)
    plugin_package.checked_path(output)
    run(root, ["cargo", "build", "--locked", "--release", "--manifest-path",
               root / "src-tauri/Cargo.toml", "--example", "plugin-package",
               "--target", target, "--target-dir", target_dir])
    suffix = ".exe" if "windows" in target else ""
    packager = target_dir / target / "release/examples" / ("plugin-package" + suffix)
    with tempfile.TemporaryDirectory(prefix=".building-", dir=output) as temporary:
        staging = Path(temporary).resolve()
        artifacts = staging / "artifacts"
        artifacts.mkdir()
        packages = []
        for plugin, (_, binary) in plugin_package.PLUGINS.items():
            manifest = root / "desktop-plugins" / plugin / "Cargo.toml"
            cargo = tomllib.loads(manifest.read_text(encoding="utf-8"))["package"]
            if cargo["name"] != binary:
                raise ValueError("Worker Cargo identity does not match package metadata")
            run(root, ["cargo", "build", "--locked", "--release", "--manifest-path",
                       manifest, "--bin", binary, "--target", target,
                       "--target-dir", target_dir])
            worker = target_dir / target / "release" / (binary + suffix)
            prepared = plugin_package.prepare(worker, target, staging / plugin, plugin)
            metadata = json.loads((prepared / "manifest.json").read_text(encoding="utf-8"))
            if metadata["version"] != cargo["version"]:
                raise ValueError("Worker Cargo version does not match package metadata")
            name = f"{metadata['plugin_id']}-{metadata['version']}-{target}.idplugin"
            archive = artifacts / name
            run(root, [packager, "--unsigned", "--manifest", prepared / "manifest.json",
                       "--root", prepared / "payload", "--output", archive])
            data = archive.read_bytes()
            if not data:
                raise ValueError("Packaging utility produced an empty archive")
            digest = hashlib.sha256(data).hexdigest()
            (artifacts / (name + ".sha256")).write_text(
                f"{digest}  {name}\n", encoding="utf-8")
            packages.append({"plugin_id": metadata["plugin_id"],
                             "version": metadata["version"], "file": name,
                             "sha256": digest})
        # An inventory, not an importable configuration: local files must not
        # masquerade as published release URLs or change installed package pins.
        inventory = {"target": target, "packages": packages}
        (artifacts / "local-plugin-artifacts.json").write_text(
            json.dumps(inventory, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        destination = output / uuid.uuid4().hex
        artifacts.rename(destination)
    return destination


def read_json(path):
    plugin_package.checked_path(path)
    if not path.is_file() or path.stat().st_size > 1024 * 1024 or path.stat().st_nlink != 1:
        raise ValueError('Expected a bounded regular JSON file')
    return json.loads(path.read_text(encoding='utf-8'))


def installed(app_data):
    state = read_json(app_data / 'plugins/state.json')
    if state.get('schema_version') != 1 or not isinstance(state.get('plugins'), dict):
        raise ValueError('Unsupported installed plugin inventory')
    return state['plugins']


def atomic_write(path, data):
    plugin_package.checked_path(path.parent)
    descriptor, temporary = tempfile.mkstemp(prefix='.local-plugins-', dir=path.parent)
    try:
        with os.fdopen(descriptor, 'wb') as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, path)
    finally:
        Path(temporary).unlink(missing_ok=True)


def artifact_identity(package, target):
    """Derive a safe basename from the built-in ID, ASCII version and target."""
    if not isinstance(package, dict):
        raise ValueError('Invalid local plugin record')
    identity, version, digest = (package.get(k) for k in ('plugin_id', 'version', 'sha256'))
    allowed = {f'inverter-desktop.{name}' for name in plugin_package.PLUGINS}
    if not all(isinstance(value, str) for value in (identity, version, digest)):
        raise ValueError('Invalid local plugin identity or digest')
    valid_version = re.fullmatch(r'\d+\.\d+\.\d+(?:[-+][\dA-Za-z.-]+)?', version, re.ASCII)
    if identity not in allowed or not valid_version or not re.fullmatch(r'[a-f0-9]{64}', digest):
        raise ValueError('Invalid local plugin identity or digest')
    name = f'{identity}-{version}-{target}.idplugin'
    if package.get('file') != name:
        raise ValueError('Invalid local archive filename')
    return identity, name, digest


def read_archive(artifacts, name, digest):
    """Read only the validated regular archive with the exact published hash."""
    archive = artifacts / name
    plugin_package.checked_path(archive)
    if not archive.is_file() or archive.stat().st_size > 64 * 1024 * 1024 or archive.stat().st_nlink != 1:
        raise ValueError('Invalid local archive file')
    content = archive.read_bytes()
    if hashlib.sha256(content).hexdigest() != digest:
        raise ValueError(f'Local archive checksum mismatch: {name}')
    return content


def read_artifacts(artifacts, target):
    """Validate the complete native inventory, including uninstalled packages."""
    if target not in plugin_package.TARGETS:
        raise ValueError('Local plugins require a supported desktop target')
    inventory = read_json(artifacts / 'local-plugin-artifacts.json')
    if not isinstance(inventory, dict) or inventory.get('target') != target:
        raise ValueError('Artifacts must target this Mac')
    packages = inventory.get('packages')
    if not isinstance(packages, list) or len(packages) != len(plugin_package.PLUGINS):
        raise ValueError('Artifacts must contain all four plugins for this Mac')
    validated, seen = [], set()
    for package in packages:
        identity, name, digest = artifact_identity(package, target)
        if identity in seen:
            raise ValueError('Duplicate local plugin identity')
        seen.add(identity)
        validated.append((package, read_archive(artifacts, name, digest)))
    return validated


def release_baseline(identity, record, previous):
    """Keep the original release pin when replacing a previous local selection."""
    active = record['active']
    if not active.get('archive_pin'):
        raise ValueError(f'{identity} is not a configuration-managed pinned package')
    baseline = active['sha256']
    old = previous.get(identity)
    if old and old['sha256'] == baseline:
        baseline = old['baseline_sha256']
    if not isinstance(baseline, str) or not re.fullmatch(r'[a-f0-9]{64}', baseline):
        raise ValueError('Invalid installed archive digest')
    return baseline


def selected_packages(archives, records, previous):
    """Do not install new plugins or change the app's enabled/disabled intent."""
    old_packages = {p['plugin_id']: p for p in previous['packages']} if previous else {}
    packages, data = [], {}
    for package, content in archives:
        identity = package['plugin_id']
        if identity not in records:
            continue
        baseline = release_baseline(identity, records[identity], old_packages)
        packages.append({**package, 'baseline_sha256': baseline})
        data[package['file']] = content
    if not packages:
        raise ValueError('No matching plugins are installed; configure them in the app first')
    return packages, data


def stage_install(artifacts, app_data, target):
    """Validate everything before atomically selecting an immutable local set."""
    archives = read_artifacts(artifacts, target)
    records = installed(app_data)
    selection = app_data / SELECTION
    previous = read_json(selection) if selection.exists() else None
    packages, data = selected_packages(archives, records, previous)
    builds = app_data / 'local-plugin-builds'
    builds.mkdir(mode=0o700, exist_ok=True)
    plugin_package.checked_path(builds)
    build = uuid.uuid4().hex
    destination = builds / build
    destination.mkdir(mode=0o700)
    try:
        for name, content in data.items():
            atomic_write(destination / name, content)
        value = {'schema_version': 1, 'target': target, 'build': build, 'packages': packages}
        atomic_write(selection, (json.dumps(value, indent=2) + '\n').encode())
    except BaseException:
        shutil.rmtree(destination)
        raise
    return {p['plugin_id']: p['sha256'] for p in packages}


def wait_installed(app_data, expected, timeout=90):
    deadline = time.monotonic() + timeout
    while True:
        records = installed(app_data)
        if all(records.get(identity, {}).get('active', {}).get('sha256') == digest
               for identity, digest in expected.items()):
            return
        if time.monotonic() >= deadline:
            raise ValueError('App did not activate the selected packages. Unlock the updated app and check Configuration > Plugins')
        time.sleep(1)


def install_plugins(artifacts, app_data, target, restore=False):
    selection = app_data / SELECTION
    if selection.exists():
        read_json(selection)
    previous = selection.read_bytes() if selection.exists() else None
    if restore:
        if previous is None:
            print('No local plugin selection is active.')
            return
        value = read_json(selection)
        records = installed(app_data)
        expected = {p['plugin_id']: p['baseline_sha256'] for p in value['packages']
                    if records.get(p['plugin_id'], {}).get('active', {}).get('sha256') == p['sha256']}
        selection.unlink()
    else:
        expected = stage_install(artifacts, app_data, target)
    try:
        print('Restarting Inverter Desktop; unlock it if prompted.', flush=True)
        restart_app()
        wait_installed(app_data, expected)
    except BaseException:
        if previous is None:
            selection.unlink(missing_ok=True)
        else:
            atomic_write(selection, previous)
        restart_app()
        raise
    print('Verified installed plugin hashes: ' + ', '.join(sorted(expected)))


def install_macos(artifacts, app_data, target, restore=False):
    """Serialize script-driven selections without locking the app's own store."""
    import fcntl
    lock = app_data / '.local-plugin-install.lock'
    plugin_package.checked_path(app_data)
    if os.path.lexists(lock):
        plugin_package.checked_path(lock)
    descriptor = os.open(lock, os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, 'r+b') as handle:
        try:
            fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise ValueError('Another local plugin installation is in progress') from error
        install_plugins(artifacts, app_data, target, restore)


def main():
    """Build the current checkout's workers for the native desktop target."""
    parser = argparse.ArgumentParser(description=__doc__)
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--install', action='store_true', help='Install into the existing macOS app after building')
    modes.add_argument('--restore-release', action='store_true', help='Restore saved release pins without compiling')
    parser.add_argument('--artifacts', type=Path, help='Reuse a previously built artifact directory with --install')
    modes.add_argument('--print-path', action='store_true', help='Print only the built directory on stdout (build diagnostics use stderr)')
    args = parser.parse_args()
    if args.artifacts and not args.install:
        parser.error('--artifacts requires --install')
    try:
        target = compiler_host(ROOT)
        if args.install or args.restore_release:
            if sys.platform != 'darwin' or not APP.is_dir():
                raise ValueError('Installation requires /Applications/Inverter Desktop.app on macOS')
            app_data = Path.home() / 'Library/Application Support/com.alvit.inverter-dashboard'
            plugin_package.checked_path(app_data)
        if args.restore_release:
            install_macos(None, app_data, target, restore=True)
            return
        destination = args.artifacts.absolute() if args.artifacts else build_plugins(ROOT, target)
        if args.print_path:
            print(destination, flush=True)
            return
        print(f'Local plugin artifacts: {destination}', flush=True)
        if args.install:
            install_macos(destination, app_data, target)
        else:
            print('Built all four packages. Add --install to activate them in the macOS app.')
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Cannot prepare local plugins: {error}\n")


if __name__ == "__main__":
    main()
