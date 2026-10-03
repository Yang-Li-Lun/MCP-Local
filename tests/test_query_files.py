"""完整排序、metadata 一致性、游標隔離及來源唯讀驗收。"""
import asyncio
import ctypes
from ctypes import wintypes
import hashlib
import json
import os
from pathlib import Path
from types import SimpleNamespace
import tempfile
import unittest
from unittest.mock import patch

import format_reader
from file_query import query_files
from local_files_mcp import create_server
from operation_budget import Budget, operation, OperationError
from workspace_reader import WorkspaceReader


def set_creation(path, seconds, extra_ticks=0):
    import msvcrt
    setter = ctypes.WinDLL('kernel32', use_last_error=True).SetFileTime
    setter.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                      ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)]
    setter.restype = wintypes.BOOL
    ticks = int((seconds + 11644473600) * 10000000) + extra_ticks
    value = wintypes.FILETIME(ticks & 0xffffffff, ticks >> 32)
    with path.open('r+b') as handle:
        if not setter(msvcrt.get_osfhandle(handle.fileno()), ctypes.byref(value), None, None):
            raise ctypes.WinError(ctypes.get_last_error())


class QueryFilesTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'root'
        self.root.mkdir()
        self.other = self.base / 'other'
        self.other.mkdir()
        self.workspace = WorkspaceReader([
            dict(id='a', path=str(self.root)), dict(id='b', path=str(self.other))], 'a')
        self.reader = self.workspace.reader(None)

    def write(self, name, data=b'x'):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def test_default_no_content_and_file_info_metadata(self):
        path = self.write('測試.BIN', b'\x00\x01')
        with patch.object(self.reader, 'open_checked', side_effect=AssertionError('content open')):
            row = self.workspace.query_files()['files'][0]
        info = self.workspace.file_info('測試.BIN')
        self.assertEqual(row, {key: info[key] for key in row})
        self.assertEqual(row['created_time'], path.stat().st_birthtime)
        self.assertEqual(row['links'], 1)
        self.assertEqual(row['extension'], '.bin')
        self.assertFalse({'format', 'sha256', 'capabilities'} & row.keys())

    def test_creation_api_fallback_never_ctime(self):
        path = self.write('old.txt')
        set_creation(path, 946684800)
        info = path.stat()
        attrs = {key: getattr(info, key) for key in dir(info) if key.startswith('st_') and 'birth' not in key}
        attrs['st_ctime'] = -999
        with self.reader.open_checked(path, binary=True, metadata=True) as handle:
            row = format_reader.basic_metadata(self.reader, path, SimpleNamespace(**attrs), handle)
        self.assertEqual(row['created_time'], 946684800)
        self.assertEqual(row['created_time_ns'], 946684800000000000)

    def test_creation_sort_preserves_windows_100ns_precision(self):
        later = self.write('a.bin')
        earlier = self.write('z.bin')
        set_creation(later, 1800000000, 1)
        set_creation(earlier, 1800000000)
        self.assertEqual(later.stat().st_birthtime_ns - earlier.stat().st_birthtime_ns, 100)
        result = self.workspace.query_files(sort_by='created_time', limit=1)
        self.assertEqual(result['files'][0]['path'], 'z.bin')

    def test_over_500_global_oldest_and_top_n_every_sort(self):
        for number in range(601):
            path = self.write(f'f{number:04}.bin', b'x' * (number % 11))
            os.utime(path, (1000000000 + number, 1100000000 + number))
            set_creation(path, 1200000000 + number)
        last = self.write('z/earliest.bin', b'z' * 99)
        set_creation(last, 946684800)
        oldest = self.workspace.query_files(sort_by='created_time', limit=1)
        self.assertEqual(oldest['files'][0]['path'], 'z/earliest.bin')
        self.assertEqual(oldest['matched_count'], 602)
        all_rows = []
        page = self.workspace.query_files(limit=500)
        all_rows.extend(page['files'])
        page = self.workspace.query_files(limit=500, cursor=page['next_cursor'])
        all_rows.extend(page['files'])
        for field in ('name', 'path', 'size', 'created_time', 'modified_time', 'accessed_time'):
            for order in ('asc', 'desc'):
                with self.subTest(field=field, order=order):
                    expected = sorted(all_rows, key=lambda r: (
                        r[field].casefold() if isinstance(r[field], str) else r[field],
                        r['path'].casefold(), r['path']), reverse=order == 'desc')[:7]
                    actual = self.workspace.query_files(sort_by=field, order=order, limit=7)['files']
                    self.assertEqual(actual, expected)

    def test_filters_inclusive_and_combined(self):
        path = self.write('Sub/Alpha.TXT', b'12345')
        self.write('beta.txt', b'123456')
        self.write('Alpha.bin', b'12345')
        os.utime(path, (1000000000, 1100000000))
        set_creation(path, 1200000000)
        result = self.workspace.query_files(name='ALPHA', extensions=['TXT'], min_size=5,
            max_size=5, created_after=1200000000, created_before=1200000000,
            modified_after=1100000000, modified_before=1100000000,
            accessed_after=1000000000, accessed_before=1000000000)
        self.assertEqual([r['path'] for r in result['files']], ['Sub/Alpha.TXT'])
        self.assertEqual(self.workspace.query_files(name='missing')['matched_count'], 0)
        self.write('noext')
        self.assertEqual(self.workspace.query_files(extensions=[''])['files'][0]['path'], 'noext')

    def test_pagination_stable_ties_no_rescan_and_query_binding(self):
        for name in ('a.txt', 'B.txt', 'c.txt', 'd.txt'):
            self.write(name)
        first = self.workspace.query_files(sort_by='size', order='desc', limit=2)
        token = first['next_cursor']
        with patch.object(self.reader, 'walk', side_effect=AssertionError('rescan')):
            second = self.workspace.query_files(sort_by='size', order='desc', limit=5, cursor=token)
        self.assertEqual([r['path'] for r in first['files'] + second['files']],
                         ['d.txt', 'c.txt', 'B.txt', 'a.txt'])
        self.assertIsNone(second['next_cursor'])
        for options in (dict(root_id='b'), dict(name='a'), dict(order='asc'),
                        dict(include_sha256=True), dict(sort_by='name'), dict(directory='missing')):
            with self.subTest(options=options), self.assertRaises(ValueError):
                self.workspace.query_files(**dict(dict(sort_by='size', order='desc', cursor=token), **options))
        for bad in ('!', token[:-4] + 'aaaa'):
            with self.assertRaises(ValueError):
                self.workspace.query_files(cursor=bad)

    def test_cursor_policy_expiry_root_replacement_and_deleted_file(self):
        self.write('a.txt')
        self.write('b.txt')
        token = self.workspace.query_files(limit=1)['next_cursor']
        self.reader.settings['excluded_names'].append('b.txt')
        with self.assertRaises(ValueError):
            self.workspace.query_files(cursor=token)
        self.reader.settings['excluded_names'].remove('b.txt')
        with patch('snapshot_cache.CACHE.clock', return_value=10**15), self.assertRaises(ValueError):
            self.workspace.query_files(cursor=token)
        token = self.workspace.query_files(limit=1)['next_cursor']
        (self.root / 'b.txt').unlink()
        with self.assertRaises(ValueError):
            self.workspace.query_files(cursor=token)
        self.root.rename(self.base / 'old-root')
        self.root.mkdir()
        with self.assertRaises(ValueError):
            self.workspace.query_files(cursor=token)

    def test_changed_and_hidden_page_rejected(self):
        self.write('a.txt')
        path = self.write('b.txt')
        token = self.workspace.query_files(limit=1)['next_cursor']
        path.write_bytes(b'changed')
        with self.assertRaisesRegex(ValueError, 'QUERY_CHANGED'):
            self.workspace.query_files(cursor=token)
        token = self.workspace.query_files(limit=1)['next_cursor']
        import win32api
        win32api.SetFileAttributes(str(path), 2)
        try:
            with self.assertRaises(ValueError):
                self.workspace.query_files(cursor=token)
        finally:
            win32api.SetFileAttributes(str(path), 128)

    def test_security_exclusions_hidden_links_and_state(self):
        self.write('safe.bin')
        for name in ('.secret.txt', 'credentials.json', 'node_modules/x.txt', 'custom/x.txt', 'state/key.txt'):
            self.write(name)
        self.reader.settings['excluded_names'].append('custom')
        os.link(self.root / 'safe.bin', self.root / 'hard.bin')
        target = self.other / 'outside.txt'
        target.write_text('outside')
        (self.root / 'link.txt').symlink_to(target)
        (self.root / 'junction').symlink_to(self.other, target_is_directory=True)
        with patch('security_policy.STATE_DIR', self.root / 'state'):
            self.assertEqual(self.workspace.query_files()['files'], [])
            for directory in ('../other', str(self.other), 'safe.bin:stream', '.secret.txt', 'node_modules',
                              'custom', 'state', 'link.txt', 'junction', 'NUL', 'x.', 'x\x00'):
                with self.subTest(directory=directory), self.assertRaises(ValueError):
                    self.workspace.query_files(directory=directory)

    def test_scan_and_snapshot_limits_fail_closed(self):
        for i in range(3):
            self.write(f'{i}.txt')
        for setting in ('max_scan_files', 'max_scan_entries'):
            old = self.reader.settings[setting]
            self.reader.settings[setting] = 1
            with self.assertRaisesRegex(ValueError, 'QUERY_SCAN_LIMIT'):
                self.workspace.query_files(limit=1)
            self.reader.settings[setting] = old
        with patch('snapshot_cache.SNAPSHOT_BYTES', 1024), self.assertRaisesRegex(ValueError, 'QUERY_SNAPSHOT_LIMIT'):
            self.workspace.query_files(limit=1)
        with patch.object(self.reader, 'walk', side_effect=PermissionError('private absolute path')):
            with self.assertRaisesRegex(ValueError, '^QUERY_UNAVAILABLE'):
                self.workspace.query_files()

    def test_opt_in_only_returned_page_and_hash_readonly(self):
        self.write('a.bin', b'\0a')
        self.write('b.bin', b'\0b')
        before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in self.root.iterdir()}
        original = format_reader.file_info
        with patch('format_reader.file_info', wraps=original) as parser:
            result = self.workspace.query_files(limit=1, include_format=True,
                include_capabilities=True, include_sha256=True)
            self.assertEqual(parser.call_count, 1)
        row = result['files'][0]
        self.assertEqual(row['sha256'], hashlib.sha256(b'\0a').hexdigest())
        self.assertIn('capabilities', row)
        self.assertIn('format', row)
        self.assertEqual(before, {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in self.root.iterdir()})
        self.reader.settings['max_file_bytes'] = 1
        self.assertEqual(self.workspace.query_files()['matched_count'], 2)
        with self.assertRaises(ValueError):
            self.workspace.query_files(include_sha256=True)

    def test_validation_and_cancel(self):
        for options in (dict(limit=True), dict(limit=501), dict(min_size=-1), dict(min_size=3, max_size=2),
                        dict(created_after=float('nan')), dict(created_before='yesterday'),
                        dict(include_format='true'), dict(sort_by='ctime'), dict(order='up'),
                        dict(extensions=['../txt']), dict(name='')):
            with self.subTest(options=options), self.assertRaises(ValueError):
                query_files(self.reader, **options)
        budget = Budget()
        with operation(budget):
            budget.cancel.set()
            with self.assertRaises(OperationError):
                self.workspace.query_files()

    def test_opt_in_page_byte_budget_and_missing_birth(self):
        self.write('a.bin', b'\x00' * 20)
        self.write('b.bin', b'\x00' * 20)
        self.reader.settings['max_scan_bytes'] = 30
        with self.assertRaises((ValueError, OperationError)):
            self.workspace.query_files(include_sha256=True)
        with self.assertRaises(OperationError):
            self.workspace.query_files(include_format=True)
        original = format_reader.basic_metadata

        def unavailable(*args, **kwargs):
            return dict(original(*args, **kwargs), created_time=None)

        with patch('format_reader.basic_metadata', side_effect=unavailable):
            self.assertIsNone(self.workspace.query_files()['files'][0]['created_time'])
            with self.assertRaisesRegex(ValueError, 'QUERY_TIME_UNAVAILABLE'):
                self.workspace.query_files(sort_by='created_time', limit=1)
            with self.assertRaisesRegex(ValueError, 'QUERY_TIME_UNAVAILABLE'):
                self.workspace.query_files(created_after=0)

    def test_contract_strict_schema_and_old_21_unchanged(self):
        server = create_server(self.workspace)
        tools = asyncio.run(server.list_tools())
        self.assertEqual(len(tools), 22)
        from tool_contract import verify_contract
        verify_contract(tools)
        model = server._tool_manager.get_tool('query_files').fn_metadata.arg_model
        for options in (dict(limit='1'), dict(limit=True), dict(unknown=1), dict(include_format=1),
                        dict(created_after='100'), dict(extensions='txt'), dict(sort_by='ctime')):
            with self.subTest(options=options), self.assertRaises(ValueError):
                model.model_validate(options)


if __name__ == '__main__':
    unittest.main()
