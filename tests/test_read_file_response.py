"""公開單檔回傳、底層相容性與 MCP 封裝的隔離回歸。"""
import asyncio
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from mcp.server.fastmcp.exceptions import ToolError
from mcp_local.local_files_mcp import create_server
from mcp_local.tool_contract import verify_contract
from mcp_local.workspace_reader import WorkspaceReader


class ReadFileResponseTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-read-response-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / 'main'
        self.other = Path(temporary.name) / 'other'
        self.root.mkdir()
        self.other.mkdir()
        (self.root / 'sample.txt').write_bytes('第一行\r\nsecond\n第三行\n'.encode('utf-8-sig'))
        (self.root / 'empty.txt').write_bytes(b'')
        (self.root / 'limit.txt').write_text(('中' * 1000 + '\n') * 30, encoding='utf-8')
        (self.other / 'sample.txt').write_text('另一根', encoding='utf-8')
        self.workspace = WorkspaceReader([
            {'id': 'main', 'path': str(self.root)},
            {'id': 'other', 'path': str(self.other)}], 'main')

    def test_public_text_preserves_every_metadata_field_and_page(self):
        for path, start, count in [('sample.txt', 1, 2), ('sample.txt', 3, 2),
                                   ('sample.txt', 99, 1), ('empty.txt', 1, 1),
                                   ('limit.txt', 1, 400)]:
            with self.subTest(path=path, start=start):
                raw = self.workspace.reader(None).read_file(path, start, count)
                result = self.workspace.read_file(path, start, count)
                self.assertEqual(result['text'], raw['content'])
                self.assertNotIn('content', result)
                self.assertEqual({k: v for k, v in result.items() if k != 'text'},
                                 {k: v for k, v in raw.items() if k != 'content'})
                self.assertLessEqual(len(result['text']), 24000)
        page = self.workspace.read_file('sample.txt', line_count=2)
        self.assertEqual(page['text'], '1: 第一行\n2: second')
        self.assertEqual(page['next_start_line'], 3)
        final = self.workspace.read_file('sample.txt', page['next_start_line'], 2)
        self.assertEqual(final['text'], '3: 第三行')
        self.assertIsNone(final['next_start_line'])
        self.assertEqual(final['total_lines'], 3)
        self.assertEqual(self.workspace.read_file('sample.txt', root_id='other')['text'], '1: 另一根')
        self.assertIsNotNone(self.workspace.read_file('limit.txt', line_count=400)['next_start_line'])

    def test_public_adaptation_does_not_mutate_underlying_result(self):
        raw = self.workspace.reader(None).read_file('sample.txt', 1, 1)
        original = dict(raw)
        with patch.object(self.workspace, 'call', return_value=raw) as call:
            result = self.workspace.read_file('sample.txt', 1, 1)
        call.assert_called_once_with(None, 'read_file', 'sample.txt', 1, 1)
        self.assertEqual(raw, original)
        self.assertIsNot(result, raw)

    def test_batch_ranges_and_project_context_keep_content(self):
        raw = self.workspace.reader(None).read_file('sample.txt', 1, 2)
        batch = self.workspace.read_files([{'path': 'sample.txt', 'line_count': 2}])
        self.assertEqual(batch, {'files': [{'index': 0, **raw, 'returned_lines': 2,
                                         'has_more': True}], 'truncated': False})
        ranges = [{'start_line': 3, 'line_count': 2}, {'start_line': 1, 'line_count': 2},
                  {'start_line': 1, 'line_count': 2}]
        self.assertEqual(self.workspace.read_file_ranges('sample.txt', ranges),
                         self.workspace.reader(None).read_file_ranges('sample.txt', ranges))
        (self.root / 'README.md').write_text('entry marker', encoding='utf-8')
        context = self.workspace.project_context()
        self.assertIn('content', context['files'][0])
        self.assertNotIn('text', context['files'][0])

    def test_registered_result_and_input_contract_in_all_public_modes(self):
        async def check():
            for mode in ('read_only', 'developer_control', 'host_control'):
                server = create_server(self.workspace, mode)
                try:
                    tools = await server.list_tools()
                    verify_contract(tools, mode)
                    tool = next(t for t in tools if t.name == 'read_file')
                    self.assertIsNone(tool.outputSchema)
                    self.assertIn('text', tool.description)
                    result = await server.call_tool('read_file', {'path': 'sample.txt', 'line_count': 1})
                    self.assertIsInstance(result, list)
                    self.assertEqual(len(result), 1)
                    self.assertEqual(result[0].type, 'text')
                    payload = json.loads(result[0].text)
                    self.assertEqual(payload, self.workspace.read_file('sample.txt', line_count=1))
                    for arguments in ({'path': 'sample.txt', 'line_count': '1'},
                                      {'path': 'sample.txt', 'line_count': True},
                                      {'path': 'sample.txt', 'line_count': 401},
                                      {'path': 'sample.txt', 'start_line': 0},
                                      {'path': 'sample.txt', 'extra': 1},
                                      {'path': 'sample.txt', 'root_id': 'unknown'}):
                        with self.subTest(mode=mode, arguments=arguments), self.assertRaises(ToolError):
                            await server.call_tool('read_file', arguments)
                finally:
                    if server._full_control:
                        server._full_control.close()
        asyncio.run(check())

    def test_public_read_still_validates_entire_file_and_limits(self):
        for name, data in [('invalid.txt', b'first\n' + b'a' * 66000 + b'\xff'),
                           ('nul.txt', b'first\n' + b'a' * 66000 + b'\x00'),
                           ('long.txt', b'x' * 24001)]:
            (self.root / name).write_bytes(data)
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.workspace.read_file(name, line_count=1)
        (self.root / '.hidden.txt').write_text('private', encoding='utf-8')
        for path in ('../outside.txt', '.hidden.txt', 'missing.txt'):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.workspace.read_file(path)
        limited = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main',
                                  {'max_file_bytes': 8})
        with self.assertRaises(ValueError):
            limited.read_file('sample.txt', line_count=1)
