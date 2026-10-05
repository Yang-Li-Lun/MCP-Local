"""完整控制邊界與 Win32 沙箱驗收；只使用自身 fixture，不碰正式通道或金鑰。"""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import patch, Mock

from access_mode import CONTROL_TOOLS
from connection_settings import normalize_connection, save_settings, load_settings, load_for_edit, build_commands
from control_edit import replace_block
from control_files import ControlledFiles, MAX_WRITE_BYTES
from control_sessions import Session, MAX_OUTPUT
from local_files_mcp import create_server
from tool_contract import verify_contract
from workspace_reader import WorkspaceReader, TOOLS
from command_fixture import command_project


class FullControlTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='mcp-control-test-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root),
                                           'excluded_names': ['private']}], 'main')
        self.server = create_server(self.workspace, 'full_control')
        self.control = self.server._full_control
        self.addCleanup(self.control.close)

    def test_contracts_and_diagnostics_match_both_modes(self):
        for mode, expected in [('read_only', set(TOOLS)), ('full_control', set(TOOLS + CONTROL_TOOLS))]:
            server = create_server(self.workspace, mode)
            if server._full_control:
                self.addCleanup(server._full_control.close)
            tools = self.workspace._registered_tools_provider()
            verify_contract(tools, mode)
            self.assertEqual({t.name for t in tools}, expected)
            diagnostics = self.workspace.server_diagnostics()
            self.assertTrue(diagnostics['consistent'])
            self.assertEqual(diagnostics['access_mode'], mode)
            self.assertEqual(self.workspace.workspace_info()['tools'], list(TOOLS if mode == 'read_only' else TOOLS + CONTROL_TOOLS))
            for tool in tools:
                if tool.name in TOOLS:
                    self.assertTrue(tool.annotations.readOnlyHint)

    def test_readonly_object_refuses_mutations_and_invalid_mode(self):
        files = ControlledFiles(self.workspace, 'read_only')
        with self.assertRaisesRegex(ValueError, 'READ_ONLY_MODE'):
            files.write('a.txt', 'x', 'create', None, None)
        with self.assertRaises(ValueError):
            create_server(self.workspace, 'invalid')
        self.assertFalse((self.root / 'a.txt').exists())

    def test_create_edit_append_move_delete_with_hashes_and_bom(self):
        raw = b'\xef\xbb\xbf' + 'alpha\r\n中文\r\n'.encode()
        (self.root / 'a.txt').write_bytes(raw)
        result = self.control.edit_block('a.txt', 'alpha\n中文', 'beta\n文字', hashlib.sha256(raw).hexdigest())
        self.assertEqual((self.root / 'a.txt').read_bytes(), b'\xef\xbb\xbf' + 'beta\r\n文字\r\n'.encode())
        result = self.control.write_file('a.txt', 'tail', 'append', result['sha256'])
        self.control.create_directory('new')
        self.control.move_file('a.txt', 'new/b.txt', result['sha256'])
        self.assertFalse((self.root / 'a.txt').exists())
        self.assertEqual(self.control.delete_file('new/b.txt', result['sha256'])['deleted'], True)
        self.assertFalse((self.root / 'new/b.txt').exists())

    def test_stale_hash_ambiguous_edit_and_overwrite_preserve_bytes(self):
        result = self.control.write_file('a.txt', 'word word')
        for call in [lambda: self.control.write_file('a.txt', 'new'),
                     lambda: self.control.write_file('a.txt', 'new', 'rewrite', '0' * 64),
                     lambda: self.control.edit_block('a.txt', 'word', 'changed', result['sha256']),
                     lambda: self.control.edit_block('a.txt', '', 'changed', result['sha256']),
                     lambda: self.control.delete_file('a.txt', '0' * 64)]:
            with self.assertRaises(ValueError):
                call()
            self.assertEqual((self.root / 'a.txt').read_text(), 'word word')
        self.control.edit_block('a.txt', 'word', 'changed', result['sha256'], 2)
        self.assertEqual((self.root / 'a.txt').read_text(), 'changed changed')

    def test_overlapping_matches_rejected(self):
        with self.assertRaises(ValueError):
            replace_block('aaa', 'aa', 'b', 2)

    def test_escape_hidden_exclusions_device_and_hardlink_denied(self):
        (self.root / 'private').mkdir()
        for path in ['../out.txt', str(self.root / 'absolute.txt'), '.env', 'private/a.txt',
                     'secrets.json', 'a.txt:stream', 'NUL.txt', 'trailing. ', 'a/../out.txt', '.']:
            with self.subTest(path=path), self.assertRaises((OSError, ValueError)):
                self.control.write_file(path, 'blocked')
        original = self.control.write_file('original.txt', 'safe')
        os.link(self.root / 'original.txt', self.root / 'link.txt')
        for path in ('original.txt', 'link.txt'):
            with self.assertRaises(ValueError):
                self.control.write_file(path, 'changed', 'rewrite', original['sha256'])
        self.assertEqual((self.root / 'original.txt').read_text(), 'safe')

    def test_hidden_attribute_and_symlink_denied(self):
        import ctypes
        target = self.root / 'hidden.txt'
        target.write_text('hidden')
        self.assertTrue(ctypes.windll.kernel32.SetFileAttributesW(str(target), 2))
        try:
            with self.assertRaises(ValueError):
                self.control.write_file('hidden.txt', 'changed', 'rewrite', hashlib.sha256(b'hidden').hexdigest())
        finally:
            ctypes.windll.kernel32.SetFileAttributesW(str(target), 0x80)
        link = self.root / 'linkdir'
        link.symlink_to(self.root / 'missing', target_is_directory=True)
        with self.assertRaises((ValueError, OSError)):
            self.control.create_directory('linkdir/new')

    def test_service_and_state_protected(self):
        import control_files
        with patch.object(control_files, 'PROJECT', self.root / 'program'):
            (self.root / 'program').mkdir()
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                self.control.write_file('program/a.py', 'pass')
        with patch('security_policy.STATE_DIR', self.root / 'state'):
            (self.root / 'state').mkdir()
            with self.assertRaises(ValueError):
                self.control.write_file('state/settings.json', '{}')

    def test_resource_limit_no_partial_new_file(self):
        with self.assertRaisesRegex(ValueError, 'WRITE_LIMIT'):
            self.control.write_file('large.txt', 'x' * (MAX_WRITE_BYTES + 1))
        self.assertFalse((self.root / 'large.txt').exists())

    def test_write_failure_rolls_back_preserves_acl_object(self):
        result = self.control.write_file('a.txt', 'old')
        inode = (self.root / 'a.txt').stat().st_ino
        with patch('control_files.os.fsync', side_effect=[OSError('fixture'), None]):
            with self.assertRaises(OSError):
                self.control.write_file('a.txt', 'replacement', 'rewrite', result['sha256'])
        self.assertEqual((self.root / 'a.txt').read_bytes(), b'old')
        self.assertEqual((self.root / 'a.txt').stat().st_ino, inode)

    def test_move_does_not_overwrite_or_move_directory(self):
        result = self.control.write_file('a.txt', 'one')
        self.control.write_file('b.txt', 'two')
        self.control.create_directory('folder')
        with self.assertRaises(ValueError):
            self.control.move_file('a.txt', 'b.txt', result['sha256'])
        with self.assertRaises(ValueError):
            self.control.move_file('folder', 'moved', result['sha256'])
        self.assertEqual((self.root / 'b.txt').read_text(), 'two')

    def test_settings_migration_is_readonly_and_unknown_version_preserved(self):
        source = {'root': str(self.root), 'roots': [{'id': 'main', 'path': str(self.root)}],
                  'default_root': 'main', 'tunnel': 'tunnel_fixture', 'settings_version': 5,
                  'access_mode': 'full_control'}
        target = self.root / 'settings.json'
        target.write_text(json.dumps(source))
        loaded = load_settings(target)
        self.assertEqual(loaded['access_mode'], 'read_only')
        self.assertEqual(loaded['settings_version'], 7)
        save_settings({**loaded, 'access_mode': 'full_control'}, target)
        self.assertEqual(load_for_edit(target).settings['access_mode'], 'full_control')
        with self.assertRaises(ValueError):
            normalize_connection({**loaded, 'access_mode': 'anything'})
        target.write_text(json.dumps({**source, 'settings_version': 99}))
        before = target.read_bytes()
        with self.assertRaises(ValueError):
            load_settings(target)
        self.assertEqual(target.read_bytes(), before)

    def test_tunnel_commands_include_mode_without_credential(self):
        for mode in ('read_only', 'full_control'):
            value = normalize_connection({'settings_version': 6, 'roots': [{'id': 'main', 'path': str(self.root)}],
                                          'default_root': 'main', 'tunnel': 'tunnel_fixture', 'access_mode': mode})
            with command_project():
                commands = build_commands(value)
            command = commands[0][commands[0].index('--mcp-command') + 1]
            self.assertIn('--access-mode ' + mode, command)
            self.assertNotIn('CONTROL_PLANE_API_KEY', command)

    def test_output_buffer_caps_long_unbroken_output(self):
        session = Session(Mock(pid=1))
        session.append('x' * (MAX_OUTPUT + 100))
        page = session.view(0, 12)
        self.assertEqual(len(session.output), MAX_OUTPUT)
        self.assertEqual(page['offset'], 100)
        self.assertEqual(page['next_offset'], 112)
        self.assertTrue(page['truncated'])

    def test_actual_appcontainer_interaction_isolation_and_cleanup(self):
        sentinel = self.root / 'host-only.txt'
        sentinel.write_text('DO_NOT_EXPOSE')
        (self.root / 'input.txt').write_text('中文 fixture', encoding='utf-8')
        host_path = str(sentinel).replace("'", "''")
        script = ("Write-Output ('INPUT:' + (Get-Content input.txt)); "
                  "if ($env:CONTROL_PLANE_API_KEY -or $env:TEST_SECRET_TOKEN) { throw 'ENV_LEAK' }; "
                  "try { [IO.File]::ReadAllText('" + host_path + "') | Out-Null; Write-Output 'ESCAPE' } "
                  "catch { Write-Output 'HOST_DENIED' }; "
                  "try { [IO.File]::WriteAllText('" + host_path + "', 'ESCAPE'); Write-Output 'WRITE_ESCAPE' } "
                  "catch { Write-Output 'HOST_WRITE_DENIED' }; "
                  "Write-Output 'READY'; $line=[Console]::ReadLine(); Write-Output ('ECHO:'+$line); "
                  "[IO.File]::WriteAllText((Join-Path $env:MCP_WORKSPACE 'result.txt'),'沙箱結果')")
        with patch.dict(os.environ, {'CONTROL_PLANE_API_KEY': 'fixture-not-real', 'TEST_SECRET_TOKEN': 'private-fixture'}):
            started = self.control.start_process(script, ['input.txt'], timeout_ms=0)
        identifier = started['session_id']
        actual = self.control.sessions.get(identifier)
        profile = actual.sandbox.profile.parent
        self.assertTrue(profile.exists())
        self.assertFalse(started['completed'])
        with self.assertRaises(ValueError):
            self.control.export_session_file(identifier, 'result.txt', 'result.txt')
        self.control.interact_with_process(identifier, 'hello\n', close_stdin=True)
        result = self.control.read_process_output(identifier, wait_ms=3000)
        self.assertTrue(result['completed'])
        self.assertEqual(result['exit_code'], 0, result)
        for expected in ('INPUT:中文 fixture', 'HOST_DENIED', 'HOST_WRITE_DENIED', 'ECHO:hello'):
            self.assertIn(expected, result['output'])
        self.assertNotIn('DO_NOT_EXPOSE', result['output'])
        self.assertNotIn('ESCAPE', result['output'])
        self.assertEqual(sentinel.read_text(), 'DO_NOT_EXPOSE')
        self.control.export_session_file(identifier, 'result.txt', 'result.txt')
        self.assertEqual((self.root / 'result.txt').read_text(encoding='utf-8'), '沙箱結果')
        self.control.close_session(identifier)
        self.assertFalse(profile.exists())
        with self.assertRaises(ValueError):
            self.control.force_terminate(identifier)

    def test_lifetime_and_terminate_own_job(self):
        first = self.control.start_process("Write-Output 'running'; Start-Sleep -Seconds 60", timeout_ms=0, lifetime_seconds=1)
        result = self.control.read_process_output(first['session_id'], wait_ms=3000)
        self.assertTrue(result['completed'])
        self.assertEqual(result['reason'], 'lifetime_limit')
        second = self.control.start_process('Start-Sleep -Seconds 60', timeout_ms=0)
        result = self.control.force_terminate(second['session_id'])
        self.assertTrue(result['completed'])
        self.assertEqual(result['reason'], 'terminated_by_client')
        with self.assertRaises(ValueError):
            self.control.force_terminate('0' * 32)

    def test_sandbox_failure_never_runs_host_fallback(self):
        with patch('control_sessions.Sandbox', side_effect=ValueError('SANDBOX_UNAVAILABLE')):
            with self.assertRaisesRegex(ValueError, 'SANDBOX_UNAVAILABLE'):
                self.control.start_process('Write-Output unsafe')
        self.assertEqual(self.control.list_sessions()['sessions'], [])
