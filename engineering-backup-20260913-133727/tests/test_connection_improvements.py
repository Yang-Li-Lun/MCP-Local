"""共用設定、遷移與受管理連線的回歸測試。"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import connection_cli
import connection_settings
import local_files_gui
from connection_settings import load_settings, save_settings, require_migration
from local_files_gui import Connection


class ConnectionImprovementsTests(unittest.TestCase):
    def setUp(self):
        import uuid
        mutex = patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex)
        mutex.start()
        self.addCleanup(mutex.stop)

    def test_generated_profile_backup(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory) / 'local-files-gui.yaml'
            profile.write_bytes(b'non-sensitive-test-profile')
            connection_settings.backup_connection_profile([
                ['client', 'init', '--profile-dir', directory, '--profile', 'local-files-gui']])
            self.assertEqual(next(Path(directory).glob('*.bak')).read_bytes(), profile.read_bytes())

    def test_shared_settings_and_backup_preserve_original(self):
        self.assertIs(connection_cli.load_settings, local_files_gui.load_settings)
        self.assertIs(connection_settings.build_commands, local_files_gui.build_commands)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / 'settings.json'
            old = {'root': str(root), 'tunnel': 'tunnel_example', 'recent': ['中文 空格 &'],
                   'reader': {'max_scan_files': 17}, 'start_hidden': True}
            original = json.dumps(old).encode()
            target.write_bytes(original)
            settings = load_settings(target)
            with self.assertRaises(ValueError):
                require_migration(settings)
            settings['settings_version'] = 1
            save_settings(settings, target)
            require_migration(load_settings(target))
            self.assertEqual(json.loads(next(root.glob('settings.json.*.bak')).read_text(encoding='utf-8')), old)
            self.assertEqual(load_settings(target)['recent'], old['recent'])
            self.assertEqual(load_settings(target)['reader']['max_scan_files'], 17)
            target.write_bytes(b'{broken')
            with self.assertRaises(ValueError):
                save_settings(settings, target)
            self.assertEqual(target.read_bytes(), b'{broken')

    def run_commands(self, commands, timeout=.3):
        connection = Connection(stage_timeout=timeout)
        with patch('connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=commands):
            connection.start({'settings_version': 1}, 'fake-secret-key')
            connection.thread.join(timeout=8)
        self.assertFalse(connection.thread.is_alive())
        self.assertIsNone(connection.job)
        return connection, list(connection.events.queue)

    def test_timeout_initialization_diagnosis_and_restart(self):
        python = sys.executable
        sleep = [python, '-c', 'import time; time.sleep(60)']
        okay = [python, '-c', 'pass']
        for commands, stage in (([sleep], '建立設定'), ([okay, sleep], '檢查連線')):
            _, events = self.run_commands(commands)
            self.assertTrue(any(kind == 'error' and stage in text and '逾時' in text for kind, text in events))
        connection, _ = self.run_commands([sleep])
        with patch('connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=[okay]):
            connection.start({'settings_version': 1}, 'fake-secret-key')
            connection.thread.join(timeout=5)
        self.assertFalse(connection.thread.is_alive())

    def test_large_output_is_bounded_and_no_credentials_exposed(self):
        program = 'import sys; sys.stdout.write("fake-secret-key Bearer other-credential 401\\n" * 100000); sys.exit(9)'
        _, events = self.run_commands([[sys.executable, '-c', program]], timeout=5)
        summary = str(events)
        self.assertNotIn('fake-secret-key', summary)
        self.assertNotIn('other-credential', summary)
        self.assertLess(len(summary), 1000)
        self.assertTrue(any(kind == 'error' and '9' in text for kind, text in events))

    def test_run_has_no_short_timeout_and_duplicate_lock(self):
        okay = [sys.executable, '-c', 'pass']
        sleep = [sys.executable, '-c', 'import time; time.sleep(60)']
        first = Connection(stage_timeout=.5)
        second = Connection()
        with patch('connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=[okay, okay, sleep]):
            first.start({'settings_version': 1}, 'fake-secret-key')
            try:
                for _ in range(3):
                    self.assertEqual(first.events.get(timeout=5)[0], 'status')
                time.sleep(.7)
                self.assertTrue(first.thread.is_alive())
                with self.assertRaises(ValueError):
                    first.start({'settings_version': 1}, 'fake-secret-key')
                second.start({'settings_version': 1}, 'fake-secret-key')
                second.thread.join(timeout=5)
                self.assertTrue(any(kind == 'error' and 'Ctrl+C' in text for kind, text in second.events.queue))
                self.assertTrue(first.thread.is_alive())
            finally:
                first.stop()
                first.thread.join(timeout=5)
        self.assertFalse(first.thread.is_alive())

    def test_migration_blocks_before_process_creation(self):
        with patch('connection_runtime.build_commands') as build:
            connection = Connection()
            connection.start({}, 'fake-secret-key')
            connection.thread.join(timeout=5)
        build.assert_not_called()
        self.assertTrue(any(kind == 'error' and '遷移' in text for kind, text in connection.events.queue))
