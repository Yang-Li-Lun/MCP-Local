"""契約 4：完整性、預算、單次讀取與低成本列舉回歸。"""
import asyncio
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch
from local_files_mcp import FileReader, create_server
from workspace_reader import WorkspaceReader
from operation_budget import ACTIVE, Budget, OperationError, asynchronous
import operation_budget
from stream_read import read_ranges, read_window
from snapshot_cache import SnapshotBuilder, SnapshotCache


class PerformanceV4Tests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.reader = FileReader(self.root)
        self.root = self.reader.root
        self.workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')

    def test_ranges_order_overlap_duplicates_and_eof(self):
        text = '\ufeff甲\r\n乙\v丙\f丁\x85戊\u2028己\r尾'
        (self.root / 'a.txt').write_text(text, encoding='utf-8', newline='')
        ranges = [dict(start_line=s, line_count=c) for s, c in [(4, 2), (1, 3), (2, 4), (1, 3), (30, 2)]]
        expected = [self.reader.read_file('a.txt', **r) for r in ranges]
        with patch.object(self.reader, 'open_checked', wraps=self.reader.open_checked) as opened:
            result = self.reader.read_file_ranges('a.txt', ranges)
        self.assertEqual(opened.call_count, 1)
        self.assertEqual(result['total_lines'], len(text.removeprefix('\ufeff').splitlines()))
        for i, (row, old) in enumerate(zip(result['ranges'], expected)):
            self.assertEqual(row['index'], i)
            self.assertEqual(row['content'], old['content'])
            self.assertEqual(row['next_start_line'], old['next_start_line'])

    def test_ranges_late_invalid_and_oversize(self):
        for suffix in [b'\x00', b'\xff', b'\xe4']:
            data = b'ok\n' + b'x' * 70000 + suffix
            with self.assertRaises(ValueError):
                read_ranges(io.BytesIO(data), len(data), [dict(start_line=1, line_count=1)])
        with self.assertRaises(ValueError):
            read_ranges(io.BytesIO(b'ok\ntail'), 4, [dict(start_line=1, line_count=1)])

    def test_ranges_validation_before_open(self):
        for ranges in [[], [{}], [{'start_line': True, 'line_count': 1}],
                       [{'start_line': 1, 'line_count': 401}],
                       [{'start_line': 1, 'line_count': 1, 'extra': 1}],
                       [{'start_line': 1, 'line_count': 1}] * 17]:
            with self.assertRaises(ValueError), patch.object(self.reader, 'open_checked') as opened:
                self.reader.read_file_ranges('a.txt', ranges)
            opened.assert_not_called()

    def test_ranges_chunk_split_crlf_and_multibyte(self):
        data = b'x' * 65535 + b'\r\n' + '中文'.encode() + b'\nend'
        result = read_ranges(io.BytesIO(data), len(data), [dict(start_line=2, line_count=2)])
        self.assertEqual(result['ranges'][0]['content'], '2: 中文\n3: end')
        self.assertEqual(result['total_lines'], 3)

    def test_ranges_long_line_and_character_cutoff(self):
        with self.assertRaises(ValueError):
            read_window(io.BytesIO(b'a' * 24000), 25000, 1, 1)
        result = read_ranges(io.BytesIO((b'a' * 12000 + b'\n') * 3), 50000,
                             [dict(start_line=1, line_count=3)])
        self.assertTrue(result['truncated'])
        self.assertEqual(result['ranges'][0]['returned_lines'], 1)
        self.assertEqual(result['ranges'][0]['next_start_line'], 2)

    def test_ranges_empty_and_final_newline(self):
        for data, count in [(b'', 0), (b'\n', 1), (b'a\r\n', 1), (b'a\r\nb', 2)]:
            result = read_ranges(io.BytesIO(data), 100, [dict(start_line=1, line_count=20)])
            self.assertEqual(result['total_lines'], count)

    def test_ranges_pretty_output_budget(self):
        (self.root / 'a.txt').write_text(('中' * 100 + '\n') * 400, encoding='utf-8')
        result = self.reader.read_file_ranges('a.txt', [dict(start_line=1, line_count=400)] * 16)
        self.assertLessEqual(len(json.dumps(result, ensure_ascii=False, indent=2).encode()), 512 * 1024)
        self.assertTrue(result['truncated'])

    def test_batch_32_and_33_and_budget(self):
        (self.root / 'a.txt').write_text(('中' * 100 + '\n') * 400, encoding='utf-8')
        result = self.workspace.read_files([dict(path='a.txt', line_count=400)] * 32)
        self.assertEqual(len(result['files']), 32)
        self.assertLessEqual(len(json.dumps(result, ensure_ascii=False, indent=2).encode()), 512 * 1024)
        self.assertTrue(result['truncated'])
        with self.assertRaises(ValueError):
            self.workspace.read_files([dict(path='a.txt')] * 33)
        (self.root / 'a.txt').write_text('ok', encoding='utf-8')
        result = self.workspace.read_files([dict(path='a.txt')] * 32)
        self.assertTrue(all('content' in r for r in result['files']))

    def test_find_early_stop_and_no_content_reads(self):
        for i in range(200):
            (self.root / f'{i:03}.txt').touch()
        with patch.object(self.reader, 'open_checked') as opened, patch('local_files_mcp.CACHE.put') as cached:
            result = self.reader.find_files(['000'], limit_per_query=1)
        self.assertEqual(result['scanned_entries'], 1)
        self.assertTrue(result['truncated'])
        self.assertEqual(result['results'][0]['matches'][0]['path'], '000.txt')
        opened.assert_not_called()
        cached.assert_not_called()

    def test_find_absent_excluded_and_limits(self):
        (self.root / '.hidden.txt').touch()
        (self.root / 'a.txt').touch()
        result = self.reader.find_files(['absent'])
        self.assertFalse(result['truncated'])
        self.assertEqual(result['results'][0]['matches'], [])
        for i in range(60):
            (self.root / f'{i}.txt').touch()
        result = self.reader.find_files(['.TXT'] * 10, limit_per_query=50)
        self.assertEqual(sum(len(g['matches']) for g in result['results']), 200)
        self.assertLessEqual(len(json.dumps(result, ensure_ascii=False, indent=2).encode()), 100 * 1024)
        for queries in [[], [''], ['x'] * 11, [True]]:
            with self.assertRaises(ValueError):
                self.reader.find_files(queries)
        with self.assertRaises(ValueError):
            self.reader.find_files(['a'], '..')

    def test_one_final_output_check_local_workspace_async(self):
        (self.root / 'a.txt').write_text('ok')
        for call in [lambda: self.reader.read_file('a.txt'), lambda: self.workspace.read_file('a.txt'),
                     lambda: asyncio.run(asynchronous(self.workspace.read_file)('a.txt'))]:
            with patch('operation_budget.ensure_output_limit', wraps=operation_budget.ensure_output_limit) as checked:
                call()
                self.assertEqual(checked.call_count, 1)

    def test_checkpoint_count_and_exact_entry_budget(self):
        for i in range(1000):
            (self.root / f'{i}.txt').touch()
        with patch('local_files_mcp.checkpoint', wraps=operation_budget.checkpoint) as checked:
            self.reader.list_files()
        self.assertLess(checked.call_count, 300)
        budget = Budget()
        budget.max_entries = 17
        token = ACTIVE.set(budget)
        try:
            with self.assertRaises(OperationError):
                self.reader.list_files()
            self.assertEqual(budget.entries, 18)
        finally:
            ACTIVE.reset(token)

    def test_cancel_and_timeout(self):
        for timeout, cancel in [(30, True), (-1, False)]:
            budget = Budget(timeout=timeout)
            if cancel:
                budget.cancel.set()
            token = ACTIVE.set(budget)
            try:
                with self.assertRaises(OperationError):
                    self.reader.find_files(['x'])
            finally:
                ACTIVE.reset(token)

    def test_snapshot_builder_immutable_and_single_estimate(self):
        builder = SnapshotBuilder()
        row = {'path': 'a.txt', 'name': 'a.txt', 'type': 'file'}
        builder.append(row)
        row['path'] = 'changed'
        status = {'truncated': False}
        cache = SnapshotCache()
        with patch('snapshot_cache.estimated_bytes', side_effect=AssertionError('recount')):
            key = cache.put('o', 'f', builder, status)
        status['truncated'] = True
        result = cache.get(key, 'o', 'f')
        self.assertEqual(result['rows'][0]['path'], 'a.txt')
        self.assertFalse(result['status']['truncated'])
        with self.assertRaises(TypeError):
            result['rows'][0]['path'] = 'bad'

    def test_new_tools_schema_and_strict_range(self):
        server = create_server(self.workspace)
        tools = asyncio.run(server.list_tools())
        self.assertEqual(len(tools), 11)
        tool = server._tool_manager.get_tool('read_file_ranges')
        with self.assertRaises(ValueError):
            tool.fn_metadata.arg_model.model_validate({'path': 'a.txt', 'ranges': [{'start_line': True, 'line_count': 1}]})
        with self.assertRaises(ValueError):
            tool.fn_metadata.arg_model.model_validate({'path': 'a.txt', 'ranges': [{'start_line': 1, 'line_count': 1, 'bad': 1}]})

    def test_100k_checkpoint_reduction_exact_count(self):
        from contextlib import nullcontext
        from types import SimpleNamespace
        import stat
        file_info = SimpleNamespace(st_mode=stat.S_IFREG)
        dir_info = SimpleNamespace(st_mode=stat.S_IFDIR)
        folders = [SimpleNamespace(name=f'd{i}', path=str(self.root / f'd{i}')) for i in range(100)]
        files = [SimpleNamespace(name=f'f{i}.txt', path='f.txt') for i in range(1000)]
        def scan(folder):
            return nullcontext(iter(folders if folder == self.root else files))
        def info(folder, entry):
            return folder / entry.name, dir_info if folder == self.root else file_info
        self.reader.settings.update(max_scan_files=100000, max_scan_entries=300000)
        budget = Budget()
        token = ACTIVE.set(budget)
        try:
            with patch('local_files_mcp.os.scandir', side_effect=scan), patch.object(
                    self.reader, 'checked', side_effect=lambda p: self.root / p), patch.object(
                    self.reader, '_entry_info', side_effect=info), patch(
                    'local_files_mcp.checkpoint', wraps=operation_budget.checkpoint) as checked:
                status = dict(truncated=False, skipped_entries=0)
                self.assertEqual(sum(1 for _ in self.reader.walk('.', status)), 100000)
                self.assertLess(checked.call_count, 60000)
            self.assertEqual(budget.entries, 100100)
        finally:
            ACTIVE.reset(token)

    def test_new_tools_reject_hardlink_and_missing_root(self):
        import os
        (self.root / 'a.txt').write_text('ok')
        os.link(self.root / 'a.txt', self.root / 'b.txt')
        with self.assertRaises(ValueError):
            self.reader.read_file_ranges('a.txt', [dict(start_line=1, line_count=1)])
        self.assertEqual(self.reader.find_files(['.txt'])['results'][0]['matches'], [])
        with self.assertRaises(ValueError):
            self.workspace.find_files(['x'], root_id='missing')
        with self.assertRaises(ValueError):
            self.workspace.read_file_ranges('a.txt', [dict(start_line=1, line_count=1)], root_id='missing')
