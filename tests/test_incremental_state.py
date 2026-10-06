"""Hash/diff/status guards plus real MCP registration and increment evidence."""
import asyncio
import hashlib
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
import incremental_state as state
from local_files_mcp import create_server
from operation_budget import Budget, OperationError, operation
from tool_contract import verify_contract
from workspace_reader import WorkspaceReader, TOOLS


class IncrementalTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(dir=Path('tests').resolve())
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        self.reader = self.workspace.reader(None)
        self.write('a.txt', 'old')
        self.write('delete.txt', 'delete')
        self.write('same.txt', 'same')

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value, encoding='utf-8')
        return path

    def test_windows_signature_uses_birth_time_for_stable_identity(self):
        info = SimpleNamespace(st_dev=1, st_ino=2, st_size=3, st_mtime_ns=4,
                               st_ctime_ns=5, st_birthtime_ns=6, st_nlink=1,
                               st_file_attributes=32)
        with patch.object(state.os, 'name', 'nt'):
            self.assertEqual(state.signature(info)[4], 6)
        with patch.object(state.os, 'name', 'nt'):
            del info.st_birthtime_ns
            self.assertEqual(state.signature(info)[4], 5)

    def test_raw_bytes_hash_batch_and_bom(self):
        raw = b'\xef\xbb\xbfhello\r\n'
        (self.root / 'bom.txt').write_bytes(raw)
        result = self.workspace.hash_files(['a.txt', 'bom.txt'])
        self.assertEqual(result['files'][1]['sha256'], hashlib.sha256(raw).hexdigest())
        self.assertEqual(result['bytes_read'], len(raw) + 3)
        self.assertEqual(result['hashed_files'], 2)

    def test_incremental_add_delete_modify_and_reuse(self):
        baseline = self.workspace.project_status()
        unchanged = self.workspace.project_status(baseline_id=baseline['baseline_id'])
        self.assertEqual(unchanged['bytes_read'], 0)
        self.assertEqual(unchanged['reused_hashes'], 3)
        self.write('a.txt', 'new')
        self.write('added.txt', 'added')
        (self.root / 'delete.txt').unlink()
        changed = self.workspace.project_status(baseline_id=baseline['baseline_id'])
        for kind, name in [('added', 'added.txt'), ('deleted', 'delete.txt'), ('modified', 'a.txt')]:
            self.assertEqual(changed['changes'][kind], [dict(path=name, type='file')])
            self.assertEqual(changed['counts'][kind], 1)
        self.assertEqual(changed['hashed_files'], 2)
        self.assertEqual(changed['reused_hashes'], 1)
        self.assertEqual(changed['bytes_read'], 8)
        forced = self.workspace.project_status(baseline_id=changed['baseline_id'], force_hash=True)
        self.assertEqual(forced['reused_hashes'], 0)
        self.assertEqual(forced['hashed_files'], 3)

    def test_compare_files_directories_and_empty_directories(self):
        self.write('left/same.txt', 'same')
        self.write('right/same.txt', 'same')
        self.write('left/change.txt', 'before')
        self.write('right/change.txt', 'after')
        self.write('left/gone.txt', 'gone')
        self.write('right/new.txt', 'new')
        (self.root / 'right/empty').mkdir()
        result = self.workspace.compare_paths('left', 'right')
        self.assertEqual(result['counts'], dict(added=2, deleted=1, modified=1, same=2))
        self.assertEqual(self.workspace.compare_paths('left/same.txt', 'right/same.txt')['counts']['same'], 1)
        self.assertEqual(self.workspace.compare_paths('left/change.txt', 'right/change.txt')['counts']['modified'], 1)

    def test_file_directory_type_change(self):
        base = self.workspace.project_status()
        (self.root / 'a.txt').unlink()
        (self.root / 'a.txt').mkdir()
        result = self.workspace.project_status(baseline_id=base['baseline_id'])
        self.assertEqual(result['counts']['modified'], 1)
        base = result
        (self.root / 'a.txt').rmdir()
        self.write('a.txt', 'new')
        self.assertEqual(self.workspace.project_status(baseline_id=base['baseline_id'])['counts']['modified'], 1)

    def test_path_and_root_inputs_rejected_without_host_path(self):
        for value in ('../a.txt', str(self.root / 'a.txt'), 'a.txt:stream', 'CON.txt', '.secret', 'build/a.txt', 'secrets.json', '', 'x\x00', 'missing.txt'):
            for call in (lambda: self.workspace.hash_files([value]),
                         lambda: self.workspace.compare_paths(value, 'a.txt'),
                         lambda: self.workspace.project_status(directory=value)):
                with self.subTest(value=value), self.assertRaises(ValueError) as error:
                    call()
                self.assertNotIn(str(self.root), str(error.exception))
        with self.assertRaises(ValueError):
            self.workspace.hash_files(['a.txt'], root_id='unknown')

    def test_invalid_direct_inputs(self):
        for paths in (None, 'a.txt', [], ['a.txt'] * 33, [False], [1]):
            with self.subTest(paths=paths), self.assertRaises(ValueError):
                self.workspace.hash_files(paths)
        for token in ('', '../x', 'a' * 31, 'g' * 32, True):
            with self.assertRaises(ValueError):
                self.workspace.project_status(baseline_id=token)
        with self.assertRaises(ValueError):
            self.workspace.project_status(force_hash='false')
        with self.assertRaises(ValueError):
            self.workspace.project_status(directory='a.txt')

    def test_exclusions_and_state_policy_still_apply(self):
        self.write('build/hidden.txt', 'secret')
        self.write('private/no.txt', 'secret')
        self.write('.dot.txt', 'secret')
        self.write('secrets.json', '{}')
        self.reader.settings['excluded_names'].append('private')
        with patch('security_policy.STATE_DIR', self.root / 'state'):
            self.write('state/deny.txt', 'secret')
            with self.assertRaises(ValueError):
                self.workspace.hash_files(['state/deny.txt'])
            result = self.workspace.project_status()
        rendered = json.dumps(result)
        for name in ('hidden.txt', 'private', '.dot.txt', 'secrets.json', 'deny.txt'):
            self.assertNotIn(name, rendered)
        self.assertEqual(result['hashed_files'], 3)

    def test_hidden_and_reparse_guards_in_hash_compare_status(self):
        original = __import__('local_files_mcp').hidden
        for guard in ('hidden', 'linked'):
            module = __import__('local_files_mcp')
            original = getattr(module, guard)
            def denied(path, info=None):
                return path.name == 'a.txt' or original(path, info)
            with patch('local_files_mcp.' + guard, side_effect=denied):
                with self.assertRaises(ValueError):
                    self.workspace.hash_files(['a.txt'])
                with self.assertRaises(ValueError):
                    self.workspace.compare_paths('a.txt', 'same.txt')
                result = self.workspace.project_status()
                self.assertNotIn('a.txt', json.dumps(result))

    def test_windows_hidden_attribute_and_hardlink(self):
        if os.name == 'nt':
            import ctypes
            path = self.root / 'a.txt'
            self.assertTrue(ctypes.windll.kernel32.SetFileAttributesW(str(path), 2))
            try:
                with self.assertRaises(ValueError):
                    self.workspace.hash_files(['a.txt'])
                self.assertNotIn('a.txt', json.dumps(self.workspace.project_status()))
            finally:
                ctypes.windll.kernel32.SetFileAttributesW(str(path), 128)
        os.link(self.root / 'a.txt', self.root / 'hard.txt')
        with self.assertRaises(ValueError):
            self.workspace.hash_files(['a.txt'])
        self.assertNotIn('a.txt', json.dumps(self.workspace.project_status()))

    def test_baseline_revoked_paths_not_reported_as_deletions(self):
        baseline = self.workspace.project_status()
        original = __import__('local_files_mcp').hidden
        with patch('local_files_mcp.hidden', side_effect=lambda path, info=None: path.name == 'a.txt' or original(path, info)):
            with self.assertRaisesRegex(ValueError, 'BASELINE_SCOPE_CHANGED'):
                self.workspace.project_status(baseline_id=baseline['baseline_id'])
        os.link(self.root / 'a.txt', self.root / 'hard.txt')
        with self.assertRaisesRegex(ValueError, 'BASELINE_SCOPE_CHANGED'):
            self.workspace.project_status(baseline_id=baseline['baseline_id'])

    def test_metadata_reuse_still_checked_opens_and_does_not_cache_text_reads(self):
        baseline = self.workspace.project_status()
        with patch.object(self.reader, 'open_checked', wraps=self.reader.open_checked) as opened:
            result = self.workspace.project_status(baseline_id=baseline['baseline_id'])
        self.assertEqual(opened.call_count, 3)
        self.assertEqual(result['bytes_read'], 0)
        with patch.object(self.reader, 'open_checked', wraps=self.reader.open_checked) as opened:
            self.workspace.read_file('a.txt')
        self.assertEqual(opened.call_count, 1)

    def test_unchanged_bytes_touch_and_same_size_replacement(self):
        baseline = self.workspace.project_status()
        path = self.root / 'a.txt'
        info = path.stat()
        path.write_text('NEW')
        os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns))
        changed = self.workspace.project_status(baseline_id=baseline['baseline_id'])
        self.assertEqual(changed['counts']['modified'], 1)
        os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns + 1000000000))
        touched = self.workspace.project_status(baseline_id=changed['baseline_id'])
        self.assertEqual(touched['counts']['modified'], 0)
        self.assertEqual(touched['hashed_files'], 1)

    def test_format_and_capacity_and_scan_limits(self):
        for raw in (b'bad\x00', b'\xff', b'\xe4'):
            (self.root / 'invalid.txt').write_bytes(raw)
            for call in (lambda: self.workspace.hash_files(['invalid.txt']),
                         lambda: self.workspace.project_status(),
                         lambda: self.workspace.compare_paths('.', '.')):
                self.assertIsInstance(call(), dict)
            with self.assertRaises(ValueError):
                self.workspace.read_file('invalid.txt')
        (self.root / 'invalid.txt').unlink()
        self.write('denied.bin', 'abc')
        self.assertEqual(self.workspace.hash_files(['denied.bin'])['hashed_files'], 1)
        for setting, limit in [('max_file_bytes', 2), ('max_scan_bytes', 2),
                               ('max_scan_files', 1), ('max_scan_entries', 1)]:
            with patch.dict(self.reader.settings, {setting: limit}):
                with self.assertRaises(ValueError):
                    self.workspace.project_status()
        with patch.dict(self.reader.settings, {'max_scan_bytes': 4}):
            with self.assertRaises(ValueError):
                self.workspace.hash_files(['a.txt', 'same.txt'])

    def test_expired_foreign_policy_and_directory_baselines(self):
        baseline = self.workspace.project_status()
        self.write('sub/a.txt', 'x')
        other = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        for call in (lambda: other.project_status(baseline_id=baseline['baseline_id']),
                     lambda: self.workspace.project_status(directory='sub', baseline_id=baseline['baseline_id'])):
            with self.assertRaisesRegex(ValueError, 'BASELINE_UNAVAILABLE'):
                call()
        with patch.dict(self.reader.settings, {'max_file_bytes': 100}):
            with self.assertRaisesRegex(ValueError, 'BASELINE_UNAVAILABLE'):
                self.workspace.project_status(baseline_id=baseline['baseline_id'])
        with patch.object(state.BASELINES, 'clock', return_value=state.BASELINES.clock() + 86401):
            with self.assertRaisesRegex(ValueError, 'BASELINE_UNAVAILABLE'):
                self.workspace.project_status(baseline_id=baseline['baseline_id'])

    def test_cache_memory_entry_eviction_and_detail_budgets(self):
        with patch.object(state, 'SNAPSHOT_BYTES', 1):
            with self.assertRaisesRegex(ValueError, 'SNAPSHOT_LIMIT'):
                self.workspace.project_status()
        with patch.object(state, 'MAX_SNAPSHOT_ENTRIES', 1):
            with self.assertRaisesRegex(ValueError, 'SNAPSHOT_LIMIT'):
                self.workspace.project_status()
        with patch.object(state, 'MAX_OWNER_BASELINES', 1):
            first = self.workspace.project_status()
            self.workspace.project_status()
            with self.assertRaisesRegex(ValueError, 'BASELINE_UNAVAILABLE'):
                self.workspace.project_status(baseline_id=first['baseline_id'])
        with patch.object(state, 'MAX_CHANGE_ITEMS', 1):
            result = self.workspace.project_status()
        self.assertTrue(result['details_truncated'])
        self.assertEqual(result['returned_count'], 1)
        self.assertEqual(result['counts']['same'], 4)

    def test_timeout_cancellation_and_hard_budget(self):
        for call in (lambda: self.workspace.hash_files(['a.txt']),
                     lambda: self.workspace.compare_paths('.', '.'),
                     lambda: self.workspace.project_status()):
            with self.assertRaisesRegex(OperationError, 'TIMEOUT'), operation(Budget(timeout=-1)):
                call()
            budget = Budget()
            budget.max_bytes = 1
            with self.assertRaisesRegex(OperationError, 'RESOURCE_LIMIT'), operation(budget):
                call()
            budget = Budget()
            budget.cancel.set()
            with self.assertRaisesRegex(OperationError, 'CANCELLED'), operation(budget):
                call()

    def test_mid_read_timeout_and_failure_does_not_publish_baseline(self):
        baseline = self.workspace.project_status()
        before = set(state.BASELINES.items)
        budget = Budget()
        real = state.checkpoint
        def timeout_after_read(**kwargs):
            if kwargs.get('bytes_read'):
                budget.deadline = 0
            real(**kwargs)
        with self.assertRaisesRegex(OperationError, 'TIMEOUT'), operation(budget), patch.object(state, 'checkpoint', side_effect=timeout_after_read):
            self.workspace.project_status(baseline_id=baseline['baseline_id'], force_hash=True)
        self.assertEqual(before, set(state.BASELINES.items))

    def test_scan_io_error_not_false_deletion(self):
        baseline = self.workspace.project_status()
        with patch('local_files_mcp.os.scandir', side_effect=PermissionError('sensitive path')):
            with self.assertRaises(ValueError) as error:
                self.workspace.project_status(baseline_id=baseline['baseline_id'])
        self.assertNotIn('sensitive', str(error.exception))

    def test_root_replacement_rejected(self):
        with patch.object(self.reader, '_root_identity', (0, 0)):
            for call in (lambda: self.workspace.hash_files(['a.txt']),
                         lambda: self.workspace.compare_paths('.', '.'),
                         lambda: self.workspace.project_status()):
                with self.assertRaises(ValueError):
                    call()

    def test_cross_root_compare_and_baseline_isolation(self):
        self.write('one/a.txt', 'one')
        self.write('two/a.txt', 'two')
        workspace = WorkspaceReader([{'id': 'one', 'path': str(self.root / 'one')},
                                     {'id': 'two', 'path': str(self.root / 'two')}], 'one')
        result = workspace.compare_paths('a.txt', 'a.txt', root_id='one', right_root_id='two')
        self.assertEqual(result['counts']['modified'], 1)
        baseline = workspace.project_status(root_id='one')
        with self.assertRaisesRegex(ValueError, 'BASELINE_UNAVAILABLE'):
            workspace.project_status(root_id='two', baseline_id=baseline['baseline_id'])

    def test_diagnostics_real_registry_contract_and_no_host_paths(self):
        self.assertFalse(self.workspace.server_diagnostics()['consistent'])
        server = create_server(self.workspace)
        tools = asyncio.run(server.list_tools())
        verify_contract(tools)
        result = self.workspace.server_diagnostics()
        self.assertTrue(result['consistent'])
        self.assertEqual(set(result['registered_tools']), set(TOOLS))
        self.assertEqual(result['actual_contract_sha256'], result['expected_contract_sha256'])
        self.assertNotIn(str(self.root), json.dumps(result))
        self.assertEqual(result['capabilities']['client_tool_discovery'], 'not_observable_by_server')
        server._tool_manager.remove_tool('hash_files')
        result = self.workspace.server_diagnostics()
        self.assertFalse(result['consistent'])
        self.assertEqual(result['missing_registration'], ['hash_files'])

    def test_diagnostics_schema_mismatch_and_missing_contract(self):
        server = create_server(self.workspace)
        server._tool_manager.get_tool('hash_files').parameters['additionalProperties'] = True
        result = self.workspace.server_diagnostics()
        self.assertFalse(result['consistent'])
        self.assertEqual(result['schema_or_annotation_mismatches'], ['hash_files'])
        with patch('capability_diagnostics.Path.open', side_effect=PermissionError('HOST_SECRET_SENTINEL')):
            result = self.workspace.server_diagnostics()
        self.assertFalse(result['consistent'])
        self.assertNotIn('HOST_SECRET_SENTINEL', json.dumps(result))

    def test_new_schema_rejects_wrong_types_unknown_fields(self):
        server = create_server(self.workspace)
        cases = [('hash_files', {'paths': 'a.txt'}), ('hash_files', {'paths': [True]}),
                 ('hash_files', {'paths': ['a.txt'], 'extra': 1}),
                 ('compare_paths', {'left': True, 'right': 'a.txt'}),
                 ('project_status', {'force_hash': 'false'}),
                 ('project_status', {'baseline_id': '../x'}),
                 ('server_diagnostics', {'shell': 'anything'})]
        for name, args in cases:
            with self.subTest(name=name, args=args), self.assertRaises(Exception):
                asyncio.run(server.call_tool(name, args))

    def test_real_stdio_contract_and_incremental_changes(self):
        async def run():
            args = ['-B', str(Path('local_files_mcp.py').resolve()), '--root', str(self.root)]
            async with stdio_client(StdioServerParameters(command=sys.executable, args=args)) as (read, write):
                async with ClientSession(read, write) as session:
                    initialized = await session.initialize()
                    self.assertEqual(initialized.serverInfo.version, '2026.10.06.1')
                    verify_contract((await session.list_tools()).tools)
                    async def call(name, args):
                        response = await session.call_tool(name, args)
                        self.assertFalse(response.isError, response)
                        return json.loads(response.content[0].text)
                    diag = await call('server_diagnostics', {})
                    self.assertTrue(diag['consistent'])
                    self.assertEqual(len(diag['registered_tools']), 22)
                    hashed = await call('hash_files', {'paths': ['a.txt']})
                    self.assertEqual(hashed['files'][0]['sha256'], hashlib.sha256(b'old').hexdigest())
                    compared = await call('compare_paths', {'left': 'a.txt', 'right': 'same.txt'})
                    self.assertEqual(compared['counts']['modified'], 1)
                    base = await call('project_status', {})
                    self.write('a.txt', 'new')
                    self.write('added.txt', 'added')
                    (self.root / 'delete.txt').unlink()
                    changed = await call('project_status', {'baseline_id': base['baseline_id']})
                    self.assertEqual({key: changed['counts'][key] for key in ('added', 'deleted', 'modified')},
                                     dict(added=1, deleted=1, modified=1))
                    self.assertEqual(changed['reused_hashes'], 1)
                    self.assertEqual(changed['bytes_read'], 8)
                    denied = await session.call_tool('hash_files', {'paths': ['../denied.txt']})
                    self.assertTrue(denied.isError)
        asyncio.run(run())


    def test_diagnostics_detects_missing_annotations_and_server_version(self):
        server = create_server(self.workspace)
        server._mcp_server.version = 'wrong'
        self.assertFalse(self.workspace.server_diagnostics()['server_version_matches'])
        self.assertFalse(self.workspace.server_diagnostics()['consistent'])
        server._tool_manager.get_tool('hash_files').annotations = None
        self.assertIn('hash_files', self.workspace.server_diagnostics()['schema_or_annotation_mismatches'])
        server.add_tool(lambda: {}, name='unexpected_test_tool')
        self.assertEqual(self.workspace.server_diagnostics()['unexpected_registered_count'], 1)

    def test_output_limit_failure_does_not_publish_baseline(self):
        before = set(state.BASELINES.items)
        with patch.object(state, 'ensure_output_limit', side_effect=OperationError('OUTPUT_LIMIT')):
            with self.assertRaisesRegex(OperationError, 'OUTPUT_LIMIT'):
                self.workspace.project_status()
        self.assertEqual(before, set(state.BASELINES.items))

    def test_file_race_detected_before_publishing(self):
        real = state.handle_signature
        count = 0
        def change(handle):
            nonlocal count
            info = real(handle)
            count += 1
            if count == 2:
                info = (info[0], info[1], info[2] + 1) + info[3:]
            return info
        before = set(state.BASELINES.items)
        with patch.object(state, 'handle_signature', side_effect=change):
            with self.assertRaisesRegex(ValueError, 'SCAN_CHANGED'):
                self.workspace.project_status()
        self.assertEqual(before, set(state.BASELINES.items))

    def test_cache_global_count_and_memory_bound(self):
        cache = state.Baselines()
        row = {'a': {'type': 'file', 'sha256': '0' * 64}}
        with patch.object(state, 'MAX_BASELINES', 1):
            token = cache.put('one', 'scope', row)
            cache.put('two', 'scope', row)
            with self.assertRaises(ValueError):
                cache.get(token, 'one', 'scope')
        cache = state.Baselines()
        with patch.object(state, 'CACHE_BYTES', state.estimated_bytes(row)):
            cache.put('one', 'scope', row)
            cache.put('two', 'scope', row)
            self.assertEqual(len(cache.items), 1)
