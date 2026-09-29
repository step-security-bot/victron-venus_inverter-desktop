"""Bundle replacement failures must preserve the previous installed app."""

import contextlib
import plistlib
import re
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parents[1] / 'scripts'
with patch.object(sys, 'path', [str(SCRIPTS), *sys.path]):
    import macos_app


class LocalAppInstallTests(unittest.TestCase):
    def setUp(self):
        self.root = Path(self.enterContext(tempfile.TemporaryDirectory())).resolve()
        self.source = self.root / 'build/Inverter Desktop.app'
        self.app = self.root / 'Applications/Inverter Desktop.app'
        for bundle, version in [(self.source, 'new'), (self.app, 'old')]:
            executable = bundle / 'Contents/MacOS' / macos_app.EXECUTABLE
            executable.parent.mkdir(parents=True)
            executable.write_text(version)
            executable.chmod(0o755)
            (bundle / 'Contents/Info.plist').write_bytes(plistlib.dumps({
                'CFBundleIdentifier': macos_app.BUNDLE_ID,
                'CFBundleExecutable': macos_app.EXECUTABLE,
            }))

    def contents(self, bundle):
        return (bundle / 'Contents/MacOS' / macos_app.EXECUTABLE).read_text()

    def command(self, arguments, **_):
        if arguments[0] == '/usr/bin/ditto':
            self.assertEqual(arguments[1], '--')
            shutil.copytree(arguments[2], arguments[3])
        else:
            self.assertEqual(arguments, ['open', str(self.app)])
        return subprocess.CompletedProcess(arguments, 0)

    def test_stages_complete_bundle_before_stopping_and_keeps_app_data(self):
        configuration = self.root / 'encrypted-config'
        configuration.write_bytes(b'unchanged configuration')

        def stop(app):
            self.assertEqual(app, self.app)
            self.assertEqual(self.contents(app), 'old')
            staged = next(self.app.parent.glob('.inverter-desktop-install-*/Inverter Desktop.app'))
            self.assertEqual(self.contents(staged), 'new')
            return True

        with patch.object(macos_app.subprocess, 'run', side_effect=self.command), \
                patch.object(macos_app, 'stop_app', side_effect=stop) as stopped:
            macos_app.install_app(self.source, self.app)
        stopped.assert_called_once_with(self.app)
        self.assertEqual(self.contents(self.app), 'new')
        self.assertEqual(configuration.read_bytes(), b'unchanged configuration')
        self.assertEqual(list(self.app.parent.iterdir()), [self.app])

    def test_failed_copy_never_stops_or_removes_installed_app(self):
        def failed_copy(arguments, **kwargs):
            self.command(arguments, **kwargs)
            raise subprocess.CalledProcessError(74, arguments)

        with patch.object(macos_app.subprocess, 'run', side_effect=failed_copy), \
                patch.object(macos_app, 'stop_app') as stopped:
            with self.assertRaises(subprocess.CalledProcessError):
                macos_app.install_app(self.source, self.app)
        stopped.assert_not_called()
        self.assertEqual(self.contents(self.app), 'old')
        self.assertEqual(list(self.app.parent.iterdir()), [self.app])

    def test_relative_bundle_starting_with_dash_uses_absolute_ditto_paths(self):
        relative_source = Path('-release.app')
        self.source.rename(self.root / relative_source)
        with contextlib.chdir(self.root), \
                patch.object(macos_app.subprocess, 'run', side_effect=self.command) as command, \
                patch.object(macos_app, 'stop_app', return_value=False) as stopped:
            macos_app.install_app(relative_source, self.app.relative_to(self.root))
        arguments = command.call_args_list[0].args[0]
        self.assertEqual(arguments[0], '/usr/bin/ditto')
        self.assertEqual(arguments[1], '--')
        self.assertEqual(arguments[2], str(self.root / relative_source))
        self.assertTrue(all(Path(value).is_absolute() for value in arguments[2:]))
        stopped.assert_called_once_with(self.app)
        self.assertEqual(self.contents(self.app), 'new')

    @unittest.skipIf(sys.platform == 'win32', 'symbolic link fixtures require POSIX')
    def test_source_and_destination_symlinks_are_rejected_before_copy(self):
        source_link = self.root / 'source-link.app'
        source_link.symlink_to(self.source, target_is_directory=True)
        destination_link = self.root / 'destination-link.app'
        destination_link.symlink_to(self.app, target_is_directory=True)
        with patch.object(macos_app.subprocess, 'run') as command, \
                patch.object(macos_app, 'stop_app') as stopped:
            for source, destination in [(source_link, self.app), (self.source, destination_link)]:
                with self.subTest(source=source, destination=destination), self.assertRaises(ValueError):
                    macos_app.install_app(source, destination)
        command.assert_not_called()
        stopped.assert_not_called()
        self.assertEqual(self.contents(self.app), 'old')

    def test_failed_promotion_restores_and_reopens_previous_app(self):
        rename = Path.rename

        def fail_promotion(path, destination):
            if path.name == self.app.name and path.parent != self.app.parent:
                raise OSError('promotion failed')
            return rename(path, destination)

        with patch.object(macos_app.subprocess, 'run', side_effect=self.command) as command, \
                patch.object(macos_app, 'stop_app', return_value=True), \
                patch.object(Path, 'rename', fail_promotion):
            with self.assertRaisesRegex(OSError, 'promotion failed'):
                macos_app.install_app(self.source, self.app)
        self.assertEqual(self.contents(self.app), 'old')
        self.assertEqual(command.call_args_list[-1].args[0], ['open', str(self.app)])
        self.assertEqual(list(self.app.parent.iterdir()), [self.app])

    def test_failed_rollback_retains_recoverable_previous_bundle(self):
        rename = Path.rename

        def fail_promotion_and_restore(path, destination):
            if path.parent != self.app.parent:
                raise OSError('destination unavailable')
            return rename(path, destination)

        with patch.object(macos_app.subprocess, 'run', side_effect=self.command), \
                patch.object(macos_app, 'stop_app', return_value=True), \
                patch.object(Path, 'rename', fail_promotion_and_restore):
            with self.assertRaisesRegex(ValueError, 'previous app retained at'):
                macos_app.install_app(self.source, self.app)
        backup = next(self.app.parent.glob('.inverter-desktop-install-*/previous.app'))
        self.assertEqual(self.contents(backup), 'old')

    def test_stop_failure_keeps_existing_bundle(self):
        with patch.object(macos_app.subprocess, 'run', side_effect=self.command), \
                patch.object(macos_app, 'stop_app', side_effect=ValueError('did not stop')):
            with self.assertRaisesRegex(ValueError, 'did not stop'):
                macos_app.install_app(self.source, self.app)
        self.assertEqual(self.contents(self.app), 'old')
        self.assertEqual(list(self.app.parent.iterdir()), [self.app])

    def test_incomplete_staged_bundle_never_stops_installed_app(self):
        def incomplete_copy(arguments, **kwargs):
            result = self.command(arguments, **kwargs)
            (Path(arguments[3]) / 'Contents/MacOS' / macos_app.EXECUTABLE).unlink()
            return result

        with patch.object(macos_app.subprocess, 'run', side_effect=incomplete_copy), \
                patch.object(macos_app, 'stop_app') as stopped:
            with self.assertRaisesRegex(ValueError, 'complete'):
                macos_app.install_app(self.source, self.app)
        stopped.assert_not_called()
        self.assertEqual(self.contents(self.app), 'old')

    def test_stop_targets_exact_executable_and_waits_for_exit(self):
        with patch.object(macos_app.subprocess, 'run', side_effect=[
                subprocess.CompletedProcess([], 0), subprocess.CompletedProcess([], 0),
                subprocess.CompletedProcess([], 1)]) as command, \
                patch.object(macos_app.time, 'sleep') as sleep:
            self.assertTrue(macos_app.stop_app(self.app))
        expected = '^' + re.escape(str(self.app / 'Contents/MacOS' / macos_app.EXECUTABLE)) + '( |$)'
        self.assertEqual(command.call_args_list[0].args[0], ['pkill', '-TERM', '-f', expected])
        self.assertEqual(command.call_args_list[1].args[0], ['pgrep', '-f', expected])
        sleep.assert_called_once_with(0.25)

    def test_process_lookup_error_does_not_claim_app_stopped(self):
        with patch.object(macos_app.subprocess, 'run', side_effect=[
                subprocess.CompletedProcess([], 0), subprocess.CompletedProcess([], 2)]):
            with self.assertRaisesRegex(ValueError, 'Could not check'):
                macos_app.stop_app(self.app)

    def test_shutdown_timeout_does_not_claim_app_stopped(self):
        with patch.object(macos_app.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                patch.object(macos_app.time, 'monotonic', side_effect=[10, 25]):
            with self.assertRaisesRegex(ValueError, 'did not stop'):
                macos_app.stop_app(self.app)


if __name__ == '__main__':
    unittest.main()
