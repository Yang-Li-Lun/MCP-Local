"""共用設定、遷移與受管理連線的回歸測試。"""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from mcp_local import connection_cli
from mcp_local import connection_settings
from mcp_local import local_files_gui
from mcp_local.connection_settings import load_settings, save_settings, require_migration
from mcp_local.local_files_gui import Connection
from command_fixture import command_project


class ConnectionImprovementsTests(unittest.TestCase):
    def setUp(self):
        import uuid
        mutex = patch('mcp_local.connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex)
        mutex.start()
        self.addCleanup(mutex.stop)

    def test_reconnect_preserves_profile_backups_and_settings(self):
        from mcp_local.workspace_settings import decode_workspace
        with command_project() as project:
            state = project / 'synthetic-state'
            folder = state / 'profiles'
            folder.mkdir(parents=True)
            profile = folder / 'local-files-gui.yaml'
            profile.write_bytes(b'non-sensitive-test-profile\r\n')
            (state / 'api-key.dpapi').write_bytes(b'synthetic-not-a-credential')
            root = project / 'shared, 中文 & folder'
            root.mkdir()
            settings = {'settings_version': 8, 'roots': [{'id': 'kept', 'name': '保留', 'path': str(root),
                        'excluded_names': ['private']}], 'default_root': 'kept', 'tunnel': 'tunnel_kept'}
            target = state / 'settings.json'
            save_settings(settings, target)
            for number in range(8):
                (folder / f'{profile.name}.{number:032x}.bak').write_bytes(str(number).encode())
                (state / f'settings.json.{number:032x}.bak').write_bytes(str(number).encode())
            snapshot = lambda: {p.relative_to(state).as_posix(): (p.read_bytes(), p.stat().st_mtime_ns)
                                for p in state.rglob('*') if p.is_file()}
            before = snapshot()
            for mode in ('read_only', 'developer_control', 'host_control'):
                commands = connection_settings.build_commands({**load_settings(target), 'access_mode': mode},
                    power_channel=('a' * 32, 123))
                self.assertEqual([command[1] for command in commands], ['doctor', 'run'])
                for command in commands:
                    self.assertNotIn('--force', command)
                    self.assertEqual(command[command.index('--profile-dir') + 1], str(folder))
                    self.assertEqual(command[command.index('--control-plane.tunnel-id') + 1], 'tunnel_kept')
                    mcp_command = command[command.index('--mcp.command') + 1]
                    self.assertIn('--access-mode ' + mode, mcp_command)
                    self.assertIn('--power-channel ' + 'a' * 32 + ' --power-owner 123', mcp_command)
                    workspace = decode_workspace(mcp_command.split(' --workspace-settings ')[1].split()[0])
                    self.assertEqual(workspace['default_root'], 'kept')
                    self.assertEqual(workspace['roots'][0]['id'], 'kept')
                    self.assertEqual(workspace['roots'][0]['excluded_names'], ['private'])
                    self.assertEqual(workspace['roots'][0]['path'], str(root.resolve()))
            self.assertEqual(snapshot(), before)

    def test_first_initialization_never_forces_existing_profile(self):
        with command_project() as project:
            settings = {'root': str(project / 'shared'), 'tunnel': 'tunnel_first'}
            Path(settings['root']).mkdir()
            commands = connection_settings.build_commands(settings)
            self.assertEqual([command[1] for command in commands], ['init', 'doctor', 'run'])
            self.assertNotIn('--force', commands[0])
            self.assertEqual(commands[0][commands[0].index('--tunnel-id') + 1], 'tunnel_first')
            mcp_command = commands[0][commands[0].index('--mcp-command') + 1]
            for command in commands[1:]:
                self.assertEqual(command[command.index('--mcp.command') + 1], mcp_command)
            self.assertFalse((project / 'synthetic-state').exists())

    def test_invalid_profile_is_not_replaced(self):
        with command_project() as project:
            folder = project / 'profiles'
            folder.mkdir()
            root = project / 'shared'
            root.mkdir()
            profile = folder / 'local-files-gui.yaml'
            profile.mkdir()
            with self.assertRaises(ValueError):
                connection_settings.build_commands({'root': str(root), 'tunnel': 'tunnel_test'}, folder)
            self.assertTrue(profile.is_dir())
            source = project / 'profile-fixture.yaml'
            source.write_bytes(b'preserve fixture')
            for kind in ('symlink', 'dangling', 'hardlink', 'directory-link'):
                with self.subTest(kind=kind):
                    location = project / kind
                    if kind == 'directory-link':
                        location.symlink_to(folder, target_is_directory=True)
                    else:
                        location.mkdir()
                        linked = location / 'local-files-gui.yaml'
                        if kind == 'hardlink':
                            linked.hardlink_to(source)
                        else:
                            linked.symlink_to(source if kind == 'symlink' else project / 'missing.yaml')
                    with self.assertRaises(ValueError):
                        connection_settings.build_commands({'root': str(root), 'tunnel': 'tunnel_test'}, location)
                    self.assertEqual(source.read_bytes(), b'preserve fixture')

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
        commands = commands + [[sys.executable, '-c', 'pass']] * (3 - len(commands))
        connection = Connection(stage_timeout=timeout)
        with patch('mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                'mcp_local.connection_runtime.build_commands', return_value=commands):
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
        with patch('mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                'mcp_local.connection_runtime.build_commands', return_value=[okay]):
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
        with patch('mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                'mcp_local.connection_runtime.build_commands', return_value=[okay, okay, sleep]):
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
        with patch('mcp_local.connection_runtime.build_commands') as build:
            connection = Connection()
            connection.start({}, 'fake-secret-key')
            connection.thread.join(timeout=5)
        build.assert_not_called()
        self.assertTrue(any(kind == 'error' and '遷移' in text for kind, text in connection.events.queue))

    def test_reconnect_doctor_failure_blocks_run(self):
        from mcp_local.connection_runtime import ConnectionErrorKind
        for doctor in ([sys.executable, '-c', 'raise SystemExit(7)'],
                       [sys.executable, '-c', 'import time; time.sleep(60)']):
            connection = Connection(stage_timeout=.5)
            with patch('mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                    'mcp_local.connection_runtime.build_commands', return_value=[doctor,
                    [sys.executable, '-c', 'raise RuntimeError("must not run")']]):
                connection.start({'settings_version': 8}, 'synthetic-key')
                connection.thread.join(timeout=8)
            self.assertFalse(connection.thread.is_alive())
            self.assertEqual(connection.error_kind, ConnectionErrorKind.DOCTOR_FAILED)
            self.assertEqual([text for kind, text in connection.events.queue if kind == 'status'], ['檢查連線'])
            self.assertIsNone(connection.job)

    def test_reconnect_run_waits_until_cancelled(self):
        connection = Connection(stage_timeout=.5)
        commands = [[sys.executable, '-c', 'pass'],
                    [sys.executable, '-c', 'import time; time.sleep(60)']]
        with patch('mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                'mcp_local.connection_runtime.build_commands', return_value=commands):
            connection.start({'settings_version': 8}, 'synthetic-key')
            try:
                self.assertEqual(connection.events.get(timeout=5), ('status', '檢查連線'))
                self.assertEqual(connection.events.get(timeout=5), ('status', '通道程序執行中'))
                time.sleep(.7)
                self.assertTrue(connection.thread.is_alive())
                self.assertIsNotNone(connection.running_since)
            finally:
                connection.stop()
                connection.thread.join(timeout=5)
        self.assertFalse(connection.thread.is_alive())
        self.assertIsNone(connection.job)
