#!/usr/bin/env python3
"""Stage a macOS app before stopping it, and retain the old bundle on failure."""

import argparse
import os
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

APP = Path('/Applications/Inverter Desktop.app')
EXECUTABLE = 'inverter-dashboard'
BUNDLE_ID = 'com.alvit.inverter-dashboard'


def stop_app(app=APP):
    """Wait for this installed executable only; fail closed on process errors."""
    pattern = '^' + re.escape(str(app / 'Contents/MacOS' / EXECUTABLE)) + '( |$)'
    result = subprocess.run(['pkill', '-TERM', '-f', pattern], check=False)
    if result.returncode not in (0, 1):
        raise ValueError('Could not stop the installed desktop app')
    was_running = result.returncode == 0
    deadline = time.monotonic() + 15
    while True:
        status = subprocess.run(['pgrep', '-f', pattern], stdout=subprocess.DEVNULL,
                                check=False).returncode
        if status == 1:
            return was_running
        if status != 0:
            raise ValueError('Could not check whether the desktop app stopped')
        if time.monotonic() >= deadline:
            raise ValueError('Desktop app did not stop; close it and retry')
        time.sleep(0.25)


def restart_app(app=APP):
    stop_app(app)
    subprocess.run(['open', str(app)], check=True)


def validate_bundle(bundle):
    executable = bundle / 'Contents/MacOS' / EXECUTABLE
    if (bundle.is_symlink() or not bundle.is_dir() or not executable.is_file()
            or not os.access(executable, os.X_OK)):
        raise ValueError('Expected a complete Inverter Desktop app bundle')
    with (bundle / 'Contents/Info.plist').open('rb') as source:
        info = plistlib.load(source)
    if (not isinstance(info, dict) or info.get('CFBundleIdentifier') != BUNDLE_ID
            or info.get('CFBundleExecutable') != EXECUTABLE):
        raise ValueError('App bundle identity does not match Inverter Desktop')


def promote_bundle(staged, app, backup, was_running):
    """Restore the previous bundle if promotion fails; retain failed rollbacks."""
    try:
        if app.exists():
            app.rename(backup)
        staged.rename(app)
    except BaseException:
        if backup.exists():
            try:
                backup.rename(app)
            except OSError as rollback:
                raise ValueError(f'App replacement failed; previous app retained at {backup}') from rollback
        if was_running:
            subprocess.run(['open', str(app)], check=True)
        raise


def install_app(bundle, app=APP):
    """Copy completely before shutdown, then replace with rollback on rename failure."""
    # Reject bundle links before resolving paths, so normalization cannot hide
    # them. Absolute operands also keep names beginning with '-' out of options.
    validate_bundle(bundle)
    if app.is_symlink() or (app.exists() and not app.is_dir()):
        raise ValueError('Installed app path must be a directory, not a link')
    bundle = bundle.resolve(strict=True)
    app = app.parent.resolve(strict=True) / app.name
    if bundle == app:
        raise ValueError('The build and installed app must be different directories')
    temporary = Path(tempfile.mkdtemp(prefix='.inverter-desktop-install-', dir=app.parent))
    staged, backup = temporary / app.name, temporary / 'previous.app'
    replaced = False
    try:
        # ditto preserves macOS bundle metadata and links. A failed copy cannot
        # touch the installed app, and renames stay on the destination volume.
        subprocess.run(['/usr/bin/ditto', '--', str(bundle), str(staged)], check=True,
                       stdout=sys.stderr)
        validate_bundle(staged)
        was_running = stop_app(app)
        promote_bundle(staged, app, backup, was_running)
        replaced = True
    finally:
        # Never erase the only old bundle if restoring its name failed.
        if replaced or not backup.exists():
            try:
                shutil.rmtree(temporary)
            except OSError:
                print(f'Could not remove app staging directory: {temporary}', file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('bundle', type=Path)
    args = parser.parse_args()
    try:
        if sys.platform != 'darwin':
            raise ValueError('App installation requires macOS')
        install_app(args.bundle)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f'Cannot install desktop app: {error}\n')


if __name__ == '__main__':
    main()
