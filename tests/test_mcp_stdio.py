"""以非敏感暫存目錄實際呼叫三個 STDIO MCP 工具。"""
import asyncio
from contextlib import asynccontextmanager, redirect_stdout
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import AsyncMock, patch

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp_local.reader_settings import encode_reader_settings
from mcp_local.tool_contract import verify_contract
from tools.diagnostics import remote_smoke_test


class McpSmokeTests(unittest.TestCase):
    def test_remote_smoke_read_only_and_host_contract_over_fixture_stdio(self):
        async def verify(root, mode):
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--root', str(root), '--access-mode', mode])

            @asynccontextmanager
            async def fixture_transport(url, *, http_client):
                self.assertEqual(url, 'https://fixture.invalid/mcp')
                async with stdio_client(parameters) as (read, write):
                    yield read, write, None

            calls = []
            original_call = ClientSession.call_tool

            async def read_only_call(session, name, arguments):
                self.assertIn(name, {'workspace_info', 'list_directory', 'read_file', 'search_text',
                                    'read_files', 'search_texts', 'find_files', 'read_file_ranges'})
                calls.append(name)
                return await original_call(session, name, arguments)

            with patch('tools.diagnostics.remote_smoke_test.streamable_http_client', fixture_transport), patch.dict(
                    os.environ, {'MCP_SMOKE_BEARER': ''}), patch.object(ClientSession, 'call_tool', read_only_call):
                await remote_smoke_test.smoke('https://fixture.invalid/mcp', 'main', mode)
            self.assertEqual(len(calls), 14)

        with tempfile.TemporaryDirectory(prefix='mcp-remote-smoke-fixture-') as directory:
            root = Path(directory)
            (root / 'README.md').write_text('唯讀連線測試成功\n', encoding='utf-8')
            (root / '.hidden.txt').write_text('denied', encoding='utf-8')
            before = {p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}
            for mode in ('read_only', 'host_control'):
                with self.subTest(mode=mode):
                    asyncio.run(verify(root, mode))
            self.assertEqual({p.name: (p.read_bytes(), p.stat().st_mtime_ns) for p in root.iterdir()}, before)

    def test_remote_smoke_cli_selects_expected_mode(self):
        for extra, mode in (([], 'read_only'), (['--access-mode', 'host_control'], 'host_control')):
            with patch.object(sys, 'argv', ['remote_smoke_test.py', '--url', 'https://fixture.invalid/mcp',
                    '--fixture-root-id', 'fixture', *extra]), patch.object(
                    remote_smoke_test, 'smoke', new_callable=AsyncMock) as smoke, redirect_stdout(io.StringIO()):
                self.assertEqual(remote_smoke_test.main(), 0)
                smoke.assert_awaited_once_with('https://fixture.invalid/mcp', 'fixture', mode)

    def test_dart_read_tools_over_stdio(self):
        async def smoke(root):
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--root', str(root),
                '--reader-settings', encode_reader_settings({'max_file_bytes': 128})])
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    verify_contract((await session.list_tools()).tools)
                    for path in ('sample.dart', 'upper.DART', 'original.txt', 'original.py'):
                        for tool, args in (
                            ('read_file', {'path': path}),
                            ('read_files', {'files': [{'path': path}]}),
                            ('read_file_ranges', {'path': path, 'ranges': [
                                {'start_line': 1, 'line_count': 1},
                                {'start_line': 2, 'line_count': 1}]}),
                        ):
                            with self.subTest(path=path, tool=tool):
                                result = await session.call_tool(tool, args)
                                self.assertFalse(result.isError)
                                self.assertIn('測試', str(result.content))
                                if tool == 'read_files':
                                    self.assertNotIn('skipped_reason', json.loads(result.content[0].text)['files'][0])
                    for path in ('bad.dart', 'nul.dart', 'large.dart', '.hidden.dart',
                                 'build/blocked.dart', '../outside.dart', 'blocked.unknown',
                                 'linked.dart', 'hardlink.dart'):
                        for tool, args in (
                            ('read_file', {'path': path, 'line_count': 1}),
                            ('read_files', {'files': [{'path': path, 'line_count': 1}]}),
                            ('read_file_ranges', {'path': path, 'ranges': [
                                {'start_line': 1, 'line_count': 1}]}),
                        ):
                            with self.subTest(path=path, tool=tool):
                                result = await session.call_tool(tool, args)
                                if tool == 'read_files':
                                    self.assertIn('skipped_reason', json.loads(result.content[0].text)['files'][0])
                                else:
                                    self.assertTrue(result.isError)

        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            root = Path(directory)
            for name in ('sample.dart', 'upper.DART', 'original.txt', 'original.py',
                         '.hidden.dart', 'blocked.unknown'):
                (root / name).write_text('// 測試\nvoid main() {}\n', encoding='utf-8')
            (root / 'bad.dart').write_bytes(b'// valid first line\n\xff')
            (root / 'nul.dart').write_bytes(b'// valid first line\n\x00')
            (root / 'large.dart').write_bytes(b'// first\n' + b'x' * 129)
            (root / 'build').mkdir()
            (root / 'build' / 'blocked.dart').write_text('// blocked', encoding='utf-8')
            (root / 'linked.dart').symlink_to(root / 'sample.dart')
            (root / 'link-target.dart').write_text('// blocked', encoding='utf-8')
            (root / 'hardlink.dart').hardlink_to(root / 'link-target.dart')
            asyncio.run(smoke(root))

    def test_pagination_over_stdio(self):
        async def smoke(root):
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--root', str(root)])
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    tools = (await session.list_tools()).tools
                    verify_contract(tools)
                    for tool in tools:
                        self.assertTrue(tool.annotations.readOnlyHint)
                        if tool.name in ('list_files', 'list_directory'):
                            self.assertIn('cursor', tool.inputSchema['properties'])
                    cursor = None
                    files = []
                    while True:
                        result = await session.call_tool('list_files', {'limit': 200, 'cursor': cursor})
                        self.assertFalse(result.isError)
                        page = json.loads(result.content[0].text)
                        files.extend(page['files'])
                        cursor = page['next_cursor']
                        if cursor is None:
                            break
                    self.assertEqual(len(files), 501)
                    self.assertEqual(len(set(files)), 501)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for index in range(501):
                (root / f'{index:04}.txt').touch()
            asyncio.run(smoke(root))

    def test_custom_settings_over_stdio(self):
        async def smoke(root):
            encoded = encode_reader_settings({'extensions': ['.custom'], 'text_names': [],
                                               'excluded_names': ['deny.custom'], 'max_file_bytes': 32})
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--root', str(root),
                '--reader-settings', encoded])
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    listing = await session.call_tool('list_files', {})
                    self.assertIn('allow.custom', str(listing.content))
                    self.assertNotIn('deny.custom', str(listing.content))
                    self.assertIn('original.txt', str(listing.content))
                    readback = await session.call_tool('read_file', {'path': 'allow.custom'})
                    self.assertFalse(readback.isError)
                    search = await session.call_tool('search_text', {'query': 'marker'})
                    self.assertIn('allow.custom', str(search.content))
                    denied = await session.call_tool('read_file', {'path': 'deny.custom'})
                    self.assertTrue(denied.isError)
                    large = await session.call_tool('read_file', {'path': 'large.custom'})
                    self.assertTrue(large.isError)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('allow.custom', 'deny.custom', 'original.txt'):
                (root / name).write_text('marker', encoding='utf-8')
            (root / 'large.custom').write_text('x' * 33, encoding='utf-8')
            asyncio.run(smoke(root))

    def test_three_tools_over_stdio(self):
        async def smoke(root):
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--root', str(root)])
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    listing = await session.list_tools()
                    self.assertEqual({tool.name for tool in listing.tools},
                                     {'list_directory', 'list_files', 'read_file', 'search_text',
                                      'workspace_info', 'list_projects', 'project_context', 'read_files', 'search_texts', 'read_file_ranges', 'find_files', 'hash_files', 'compare_paths', 'project_status', 'server_diagnostics', 'read_image', 'file_info', 'read_document', 'inspect_media', 'inspect_archive', 'read_binary', 'query_files'})
                    for name, args in [('list_directory', {}), ('list_files', {}), ('read_file', {'path': 'README.md'}),
                                       ('search_text', {'query': 'MCP_GUI_SMOKE'}),
                                       ('search_texts', {'queries': ['MCP_GUI_SMOKE', 'absent']}),
                                       ('find_files', {'queries': ['README']}),
                                       ('read_file_ranges', {'path': 'README.md', 'ranges': [{'start_line': 1, 'line_count': 1}]})]:
                        result = await session.call_tool(name, args)
                        self.assertFalse(result.isError)
                        self.assertIn('README.md', str(result.content))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'README.md').write_text('MCP_GUI_SMOKE', encoding='utf-8')
            asyncio.run(smoke(root))
