"""受信任主機模式；所有副作用限定於自行建立的 fixture。"""
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

from host_control import HostControl
from host_windows import host_environment
from workspace_reader import WorkspaceReader


class HostControlTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-host-control-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root, self.outside = self.base / 'authorized', self.base / 'outside'
        self.root.mkdir()
        self.outside.mkdir()
        self.workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        self.control = HostControl(self.workspace)
        self.addCleanup(self.control.close)

    def test_absolute_file_operations_cross_original_root(self):
        path = str(self.outside / 'example.txt')
        created = self.control.write_file(path, 'original')
        self.assertEqual(created['path'], path)
        changed = self.control.edit_block(path, 'original', 'updated', created['sha256'])
        self.assertIn('updated', self.control.read_host_file(path)['content'])
        moved = str(self.root / 'moved.txt')
        self.control.move_file(path, moved, changed['sha256'])
        self.assertFalse(Path(path).exists())
        self.control.delete_file(moved, changed['sha256'])
        self.assertFalse(Path(moved).exists())
        self.control.create_directory(str(self.outside / 'created'))
        self.control.write_file('relative.txt', 'default root')
        self.assertEqual((self.root / 'relative.txt').read_text(), 'default root')

    def test_host_listing_pages_and_rejects_wrong_directory_cursor(self):
        for index in range(3):
            (self.outside / f'{index}.txt').write_text('fixture')
        first = self.control.list_host_directory(str(self.outside), limit=1)
        second = self.control.list_host_directory(str(self.outside), limit=1, cursor=first['next_cursor'])
        self.assertNotEqual(first['entries'], second['entries'])
        with self.assertRaises(ValueError):
            self.control.list_host_directory(str(self.root), cursor=first['next_cursor'])

    def test_protected_state_program_and_ambiguous_paths_denied(self):
        state, program = self.base / 'state', self.base / 'program'
        state.mkdir()
        program.mkdir()
        (state / 'credential.txt').write_text('synthetic only')
        with patch('security_policy.STATE_DIR', state), patch('control_files.PROJECT', program):
            with self.assertRaises(ValueError):
                self.control.read_host_file(str(state / 'credential.txt'))
            for path in (state / 'new.txt', program / 'new.txt'):
                with self.assertRaises(ValueError):
                    self.control.write_file(str(path), 'denied')
        for path in ('../escape.txt', 'C:relative.txt', '\\rooted.txt',
                     '\\\\server\\share\\file.txt', '\\\\?\\C:\\file.txt',
                     str(self.outside / 'file.txt') + ':stream', str(self.outside / '.env')):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.control.write_file(path, 'denied')
        for name in ('.private', 'node_modules', 'secrets.json'):
            nested = self.outside / name / 'nested'
            nested.mkdir(parents=True)
            with self.subTest(ancestor=name), self.assertRaises(ValueError):
                self.control.write_file(str(nested / 'ordinary.txt'), 'denied')
            self.assertFalse((nested / 'ordinary.txt').exists())

    def test_environment_has_no_arbitrary_credentials(self):
        with patch.dict(os.environ, {'CONTROL_PLANE_API_KEY': 'synthetic', 'OPENAI_API_KEY': 'synthetic',
                                     'TEST_SECRET_TOKEN': 'synthetic', 'PATH': 'fixture-tools'}):
            env = host_environment(self.outside, Path(os.environ['SystemRoot']))
        self.assertEqual(env['PATH'], 'fixture-tools')
        self.assertFalse(any(key in env for key in ('CONTROL_PLANE_API_KEY', 'OPENAI_API_KEY', 'TEST_SECRET_TOKEN')))

    def test_real_host_command_current_user_and_external_fixture(self):
        target = str(self.outside / 'command.txt').replace("'", "''")
        python = str(Path(sys.base_prefix) / 'python.exe').replace("'", "''")
        script = ("if ($env:CONTROL_PLANE_API_KEY -or $env:TEST_SECRET_TOKEN) {throw 'ENV_LEAK'}; "
                  "[IO.File]::WriteAllText('" + target + "','host fixture'); "
                  "& '" + python + "' -I -B -c \"import ctypes; print('ELEVATED='+str(bool(ctypes.windll.shell32.IsUserAnAdmin())))\"; "
                  "Write-Output 'READY'; Write-Output ('ECHO:'+[Console]::ReadLine())")
        with patch.dict(os.environ, {'CONTROL_PLANE_API_KEY': 'synthetic', 'TEST_SECRET_TOKEN': 'synthetic'}):
            started = self.control.start_process(script, timeout_ms=0)
        self.control.interact_with_process(started['session_id'], 'hello\n', True)
        output = self.control.read_process_output(started['session_id'], wait_ms=3000)
        self.assertTrue(output['completed'], output)
        self.assertEqual(output['exit_code'], 0, output)
        self.assertIn('ELEVATED=False', output['output'])
        self.assertIn('ECHO:hello', output['output'])
        self.assertEqual((self.outside / 'command.txt').read_text(), 'host fixture')
        self.assertEqual(output['sandbox'], 'host_process')
        closed = self.control.close_session(started['session_id'])
        self.assertFalse(closed['profile_removed'])
        self.assertTrue(self.outside.exists())

    def test_close_revokes_host_files_and_terminates_active_session(self):
        session = self.control.start_process('Start-Sleep -Seconds 60', timeout_ms=0)
        item = self.control.sessions.get(session['session_id'])
        self.control.close()
        self.assertTrue(item.closed)
        with self.assertRaises(ValueError):
            self.control.write_file(str(self.outside / 'after.txt'), 'denied')
        with self.assertRaises(ValueError):
            self.control.read_process_output(session['session_id'])
        with self.assertRaises(ValueError):
            self.control.start_process("Write-Output 'denied'")

    def test_failed_token_check_never_resumes_command(self):
        from host_windows import HostProcess
        process = HostProcess(self.outside)
        self.addCleanup(process.close)
        target = str(self.outside / 'never.txt').replace("'", "''")
        with patch.object(process, 'verify_current_token', side_effect=ValueError('fixture token rejection')):
            with self.assertRaisesRegex(ValueError, 'fixture token rejection'):
                process.start("[IO.File]::WriteAllText('" + target + "','denied')")
        self.assertFalse((self.outside / 'never.txt').exists())


if __name__ == '__main__':
    unittest.main()
