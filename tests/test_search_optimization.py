"""搜尋快路徑的完整性與狀態目錄隔離等價性。"""
import asyncio
import itertools
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from mcp_local import file_query
from mcp_local import format_reader
from mcp_local.local_files_mcp import create_server
from mcp_local import security_policy
from mcp_local.tool_contract import verify_contract
from mcp_local.workspace_reader import WorkspaceReader


class SearchOptimizationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.workspace = WorkspaceReader([dict(id='fixture', path=str(self.root))], 'fixture')
        self.reader = self.workspace.reader(None)

    def test_name_and_extension_filters_preserve_checks(self):
        for name in ('Straße.TXT', 'other.bin', 'noext'):
            (self.root / name).write_bytes(b'x')
        with patch('mcp_local.file_query.checked_stat', wraps=file_query.checked_stat) as checked:
            with patch('mcp_local.format_reader.basic_metadata', wraps=format_reader.basic_metadata) as metadata:
                result = self.workspace.query_files(name='STRASSE', extensions=['TXT', '.txt'])
        self.assertEqual([r['path'] for r in result['files']], ['Straße.TXT'])
        self.assertEqual(metadata.call_count, 1)
        # Three scan checks and two page checks, including the nonmatching files.
        self.assertEqual(checked.call_count, 5)
        self.assertEqual(self.workspace.query_files(extensions=[''])['files'][0]['path'], 'noext')

    def test_nonmatching_file_still_fails_on_security_change(self):
        (self.root / 'other.txt').write_text('fixture', encoding='utf-8')
        original = file_query.checked_stat

        def changed(reader, relative):
            if relative == 'other.txt':
                raise ValueError('QUERY_CHANGED: fixture')
            return original(reader, relative)

        with patch('mcp_local.file_query.checked_stat', side_effect=changed):
            with self.assertRaisesRegex(ValueError, 'QUERY_CHANGED'):
                self.workspace.query_files(name='missing')

    def test_no_match_keeps_scan_limit_and_metadata_error(self):
        for name in ('a.txt', 'b.txt'):
            (self.root / name).write_bytes(b'x')
        self.reader.settings['max_scan_files'] = 1
        with self.assertRaisesRegex(ValueError, 'QUERY_SCAN_LIMIT'):
            self.workspace.query_files(name='missing')
        self.reader.settings['max_scan_files'] = 3000
        with patch('mcp_local.file_query.checked_stat', side_effect=PermissionError('fixture')):
            with self.assertRaisesRegex(ValueError, 'QUERY_UNAVAILABLE'):
                self.workspace.query_files(name='missing')

    def test_path_sort_and_cursor_preserve_case_ties(self):
        for name in ('Z.txt', 'ä.txt', 'a.txt', 'sub/z.txt', 'sub/B.txt'):
            path = self.root / name
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(b'x')
        for order in ('asc', 'desc'):
            options = dict(order=order, limit=2)
            rows = []
            while True:
                result = self.workspace.query_files(**options)
                rows.extend(row['path'] for row in result['files'])
                if not result['has_more']:
                    break
                options['cursor'] = result['next_cursor']
            expected = sorted(rows, key=lambda p: (p.casefold(), p), reverse=order == 'desc')
            self.assertEqual(rows, expected)
            self.assertEqual(len(set(rows)), 5)

    def test_policy_matches_pathlib_for_lexical_boundaries(self):
        roots = [Path('C:/Users/Fixture/MCP-Local'), Path('D:/'), Path('//server/share/state')]
        tails = ['', '/child', '/child/deep', '-other', '2/child', '/../outside', '/.hidden']
        paths = [Path(str(root) + tail) for root, tail in itertools.product(roots, tails)]
        paths += [Path(str(path).swapcase()) for path in paths]
        paths += [Path('C:relative'), Path('relative/path'), Path('C:/Users/Fixture')]
        for root, path in itertools.product(roots, paths):
            with self.subTest(root=root, path=path), patch.object(security_policy, 'STATE_DIR') as state:
                state.resolve.return_value = root
                if path.is_relative_to(root):
                    with self.assertRaises(ValueError):
                        security_policy.validate_read_policy(path)
                else:
                    security_policy.validate_read_policy(path)
                state.resolve.assert_called_once_with()

    def test_policy_resolves_each_time_and_propagates_failure(self):
        first, second = self.root / 'first', self.root / 'second'
        with patch.object(security_policy, 'STATE_DIR') as state:
            state.resolve.side_effect = [first, second, PermissionError('fixture')]
            security_policy.validate_read_policy(second / 'key')
            with self.assertRaises(ValueError):
                security_policy.validate_read_policy(second / 'key')
            with self.assertRaises(PermissionError):
                security_policy.validate_read_policy(first)

    def test_readonly_schema_and_routing_instructions(self):
        server = create_server(self.workspace)
        tools = asyncio.run(server.list_tools())
        verify_contract(tools)
        self.assertEqual(len(tools), 22)
        self.assertIn('limit 只限制回傳頁', server.instructions)
        self.assertIn('context_lines', server.instructions)


if __name__ == '__main__':
    unittest.main()
