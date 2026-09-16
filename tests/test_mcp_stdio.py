"""以非敏感暫存目錄實際呼叫三個 STDIO MCP 工具。"""
import asyncio
import json
from pathlib import Path
import sys
import tempfile
import unittest

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from reader_settings import encode_reader_settings
from tool_contract import verify_contract


class McpSmokeTests(unittest.TestCase):
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
                    self.assertNotIn('original.txt', str(listing.content))
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
                                      'workspace_info', 'list_projects', 'project_context', 'read_files', 'search_texts'})
                    for name, args in [('list_directory', {}), ('list_files', {}), ('read_file', {'path': 'README.md'}),
                                       ('search_text', {'query': 'MCP_GUI_SMOKE'}),
                                       ('search_texts', {'queries': ['MCP_GUI_SMOKE', 'absent']})]:
                        result = await session.call_tool(name, args)
                        self.assertFalse(result.isError)
                        self.assertIn('README.md', str(result.content))
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'README.md').write_text('MCP_GUI_SMOKE', encoding='utf-8')
            asyncio.run(smoke(root))
