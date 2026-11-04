"""WPF 啟動與父程序退出的真實隔離生命週期測試。"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from mcp_local import desktop_launcher


class DesktopLauncherTests(unittest.TestCase):
    def test_native_tray_option_requires_fixture_and_is_forwarded(self):
        with patch('mcp_local.desktop_launcher.subprocess.Popen') as spawn:
            with self.assertRaises(SystemExit) as rejected:
                desktop_launcher.main(argv=['--fixture-native-tray'])
            self.assertEqual(rejected.exception.code, 2)
            spawn.assert_not_called()
            spawn.return_value.__enter__.return_value.wait.return_value = 0
            self.assertEqual(desktop_launcher.main(argv=['--fixture', 'synthetic-tray-fixture', '--fixture-native-tray']), 0)
            self.assertIn('--fixture-native-tray', spawn.call_args.args[0])
        backend = subprocess.run([sys.executable, '-B', '-m', 'mcp_local.desktop_bridge', '--fixture-native-tray'],
                                 cwd=desktop_launcher.PROJECT, capture_output=True, timeout=10,
                                 creationflags=subprocess.CREATE_NO_WINDOW)
        self.assertEqual(backend.returncode, 2)
        self.assertIn(b'fixture options require --fixture', backend.stderr)

    def test_launch_arguments_do_not_load_settings_or_credentials(self):
        with patch('mcp_local.desktop_launcher.subprocess.Popen') as spawn, \
             patch('mcp_local.connection_settings.load_for_edit', side_effect=AssertionError('settings read')), \
             patch('mcp_local.key_store.KeyStore', side_effect=AssertionError('credential read')):
            spawn.return_value.__enter__.return_value.wait.return_value = 0
            self.assertEqual(desktop_launcher.main(startup=True, argv=[]), 0)
            command = spawn.call_args.args[0]
            self.assertEqual(Path(command[0]).name, 'McpLocal.Desktop.exe')
            self.assertIn('--startup', command)
            self.assertIn('--watch-owner-stdin', command)

    def test_ending_launcher_closes_owned_wpf_and_backend(self):
        from mcp_local.power_windows import WindowsPower
        api = WindowsPower()  # Only OpenProcess / Wait / Close; no power mutation.
        with tempfile.TemporaryDirectory(prefix='mcp-wpf-owner-') as fixture:
            process = subprocess.Popen([sys.executable, '-B', '-m', 'mcp_local.desktop_launcher', '--fixture', fixture],
                                       cwd=desktop_launcher.PROJECT, creationflags=subprocess.CREATE_NO_WINDOW)
            handles = []
            try:
                marker = Path(fixture) / 'processes.json'
                deadline = time.monotonic() + 15
                while not marker.exists() and time.monotonic() < deadline:
                    time.sleep(.05)
                self.assertTrue(marker.exists(), 'WPF did not start its isolated backend')
                pids = json.loads(marker.read_text(encoding='utf-8'))
                for pid in pids.values():
                    handle = api.open_process(0x100000, False, pid)
                    self.assertTrue(handle)
                    handles.append(handle)
                process.terminate()  # This is only the test-owned launcher, never an existing GUI.
                process.wait(timeout=5)
                for handle in handles:
                    self.assertEqual(api.wait_one(handle, 15000), 0, 'Owned desktop/backend remained after launcher EOF')
            finally:
                if process.poll() is None:
                    process.terminate()
                    process.wait(timeout=5)
                for handle in handles:
                    api.close(handle)


if __name__ == '__main__':
    unittest.main()
