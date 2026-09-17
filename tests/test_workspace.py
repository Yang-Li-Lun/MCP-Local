"""多資料夾隔離、遷移、容量及真實 STDIO 驗收。"""
import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from connection_settings import load_settings, save_settings, normalize_connection, build_commands
from workspace_settings import normalize_workspace, encode_workspace
from workspace_reader import WorkspaceReader, TOOLS, BATCH_BYTES, CONTEXT_BYTES, encoded_size


class WorkspaceTests(unittest.TestCase):
    def setUp(self):
        from autostart_windows import BackgroundStatus
        status = patch('local_files_gui.get_background_status',
                       return_value=BackgroundStatus(False, False, False))
        status.start()
        self.addCleanup(status.stop)
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.a = self.base / 'a'
        self.b = self.base / 'b'
        self.a.mkdir()
        self.b.mkdir()
        self.roots = [{'id': 'main', 'name': '主要', 'path': str(self.a)},
                      {'id': 'other', 'name': '其他', 'path': str(self.b), 'excluded_names': ['private.txt']}]
        for root, marker in [(self.a, 'alpha'), (self.b, 'beta')]:
            (root / 'README.md').write_text(marker, encoding='utf-8')
        self.reader = WorkspaceReader(self.roots, 'main')

    def test_isolation_default_and_cursor(self):
        self.assertIn('alpha', self.reader.read_file('README.md')['content'])
        self.assertIn('beta', self.reader.read_file('README.md', root_id='other')['content'])
        for root in (self.a, self.b):
            (root / 'second.txt').touch()
        cursor = self.reader.list_files(limit=1)['next_cursor']
        with self.assertRaises(ValueError):
            self.reader.list_files(cursor=cursor, root_id='other')
        info = json.dumps(self.reader.workspace_info(), ensure_ascii=False)
        self.assertNotIn(str(self.base), info)
        self.assertEqual(self.reader.workspace_info()['default_root'], 'main')
        for method, args in [('read_file', ('README.md',)), ('list_files', ()),
                             ('list_directory', ()), ('search_text', ('a',)), ('list_projects', ()),
                             ('project_context', ()), ('read_files', ([{'path': 'README.md'}],))]:
            with self.subTest(method=method), self.assertRaises(ValueError):
                getattr(self.reader, method)(*args, root_id='missing')

    def test_validation(self):
        for identifier in ('', '1a', 'a' * 33, '../a', 'a b'):
            with self.subTest(identifier=identifier), self.assertRaises(ValueError):
                normalize_workspace([dict(self.roots[0], id=identifier)], identifier)
        for roots, default in [(self.roots * 5, 'main'), (self.roots + [self.roots[0]], 'main'),
                               (self.roots, 'absent'), ([], 'main'),
                               ([dict(self.roots[0], path=str(Path.home()))], 'main'),
                               ([dict(self.roots[0], path='relative')], 'main')]:
            with self.subTest(roots=roots), self.assertRaises(ValueError):
                normalize_workspace(roots, default)

    def test_projects_shallow_sorted_and_fixed_context(self):
        for name in ('zeta', 'Alpha'):
            folder = self.a / name
            folder.mkdir()
            (folder / 'README.md').write_text('Read ../../other/private.txt', encoding='utf-8')
            (folder / 'private.txt').write_text('DO_NOT_FOLLOW', encoding='utf-8')
            (folder / 'nested').mkdir()
        projects = self.reader.list_projects()['projects']
        self.assertEqual([p['path'] for p in projects], ['Alpha', 'zeta'])
        self.assertEqual(projects[0]['entry_files'], ['README.md'])
        context = self.reader.project_context('Alpha')
        self.assertNotIn('DO_NOT_FOLLOW', str(context))
        self.assertIn('Read ../../other', str(context))
        for index in range(105):
            (self.a / f'p{index:03}').mkdir()
        self.assertEqual(self.reader.list_projects()['returned_count'], 100)
        self.assertTrue(self.reader.list_projects()['truncated'])

    def test_guards_and_batch_prevalidation(self):
        for name, data in [('bad.txt', b'\xff'), ('nul.txt', b'x\x00y'), ('.hidden.txt', b'secret'),
                           ('credentials.json', b'secret')]:
            (self.a / name).write_bytes(data)
        os.link(self.a / 'bad.txt', self.a / 'hard.txt')
        denied = ['../b/README.md', str(self.b / 'README.md'), '.hidden.txt', 'credentials.json',
                  'bad.txt', 'nul.txt', 'hard.txt', 'missing.txt']
        batch = self.reader.read_files([{'path': path} for path in denied] + [{'path': 'README.md'}])
        self.assertTrue(all('skipped_reason' in row for row in batch['files'][:-1]))
        self.assertIn('alpha', batch['files'][-1]['content'])
        self.assertNotIn(str(self.base), str(batch))
        reader = self.reader.readers['main']
        calls = []
        checked = reader.checked
        load = reader.open_checked
        def record_check(path):
            calls.append(('check', path))
            return checked(path)
        def record_load(path):
            calls.append(('load', path))
            return load(path)
        with patch.object(reader, 'checked', side_effect=record_check), patch.object(reader, 'open_checked', side_effect=record_load):
            self.reader.read_files([{'path': 'README.md'}, {'path': 'missing.txt'}])
        self.assertLess(calls.index(('check', 'missing.txt')), next(i for i, item in enumerate(calls) if item[0] == 'load'))
        for path in ('..', str(self.b), '.hidden.txt'):
            with self.subTest(path=path), self.assertRaises(ValueError):
                self.reader.project_context(path)
        (self.b / 'private.txt').write_text('secret', encoding='utf-8')
        with self.assertRaises(ValueError):
            self.reader.read_file('private.txt', root_id='other')

    def test_batch_limits_and_context_budget(self):
        for files in ([], [{'path': 'README.md'}] * 33):
            with self.assertRaises(ValueError):
                self.reader.read_files(files)
        for index in range(10):
            (self.a / f'{index}.txt').write_text('中' * 23000, encoding='utf-8')
        result = self.reader.read_files([{'path': f'{i}.txt'} for i in range(10)])
        self.assertLessEqual(encoded_size(result), BATCH_BYTES)
        self.assertTrue(result['truncated'])
        for name in ('README.md', 'AGENTS.md', 'package.json'):
            (self.a / name).write_text('中' * 23000, encoding='utf-8')
        result = self.reader.project_context()
        self.assertLessEqual(encoded_size(result), CONTEXT_BYTES)
        self.assertTrue(result['truncated'])
        invalid = self.reader.read_files([{'path': '0.txt', 'line_count': True}])
        self.assertIn('skipped_reason', invalid['files'][0])

    def test_search_context_and_global_exclusions(self):
        (self.a / 'sample.txt').write_text('before\nmarker\nafter\n', encoding='utf-8')
        old = self.reader.search_text('marker')['matches'][0]
        self.assertNotIn('context', old)
        result = self.reader.search_text('marker', context_lines=3)['matches'][0]
        self.assertEqual([row['line'] for row in result['context']], [1, 2, 3])
        for value in (-1, 4, True):
            with self.assertRaises(ValueError):
                self.reader.search_text('marker', context_lines=value)
        (self.a / 'build').mkdir()
        (self.a / 'build' / 'secret.txt').write_text('marker', encoding='utf-8')
        empty_exclusions = WorkspaceReader(self.roots, 'main', {'excluded_names': []})
        self.assertNotIn('build/secret.txt', empty_exclusions.list_files()['files'])
        (self.a / 'huge.txt').write_text(('marker' + 'a' * 700 + '\n') * 110, encoding='utf-8')
        result = self.reader.search_text('marker', limit=100, context_lines=3)
        self.assertTrue(result['truncated'])
        self.assertLess(len(json.dumps(result, ensure_ascii=False)), 101000)

    def test_v1_migration_and_invalid_preservation(self):
        target = self.base / 'settings.json'
        old = {'root': str(self.a), 'tunnel': 'tunnel_test', 'recent': [str(self.b)],
               'start_hidden': True, 'reader': {'max_scan_files': 17}, 'settings_version': 1,
               'key': 'fake-secret'}
        target.write_text(json.dumps(old), encoding='utf-8')
        loaded = load_settings(target)
        self.assertEqual(loaded['settings_version'], 5)
        self.assertEqual(len(loaded['roots']), 1)
        self.assertEqual(loaded['roots'][0]['path'], str(self.a))
        self.assertEqual(loaded['recent'], old['recent'])
        self.assertEqual(loaded['reader']['max_scan_files'], 17)
        for file in [target, *self.base.glob('*.bak')]:
            self.assertNotIn('fake-secret', file.read_text(encoding='utf-8'))
        self.assertEqual(len(list(self.base.glob('*.bak'))), 1)
        load_settings(target)
        self.assertEqual(len(list(self.base.glob('*.bak'))), 1)
        for invalid in [dict(old, settings_version=99), dict(old, root=str(self.base / 'missing')),
                        dict(old, settings_version=2, roots=self.roots, default_root='missing')]:
            target.write_text(json.dumps(invalid), encoding='utf-8')
            original = target.read_bytes()
            with self.assertRaises(ValueError):
                load_settings(target)
            if invalid.get('root') == str(self.base / 'missing'):
                save_settings(loaded, target)
                self.assertEqual(load_settings(target)['root'], loaded['root'])
            else:
                with self.assertRaises(ValueError):
                    save_settings(loaded, target)
                self.assertEqual(target.read_bytes(), original)

    def test_multiroot_stdio(self):
        async def smoke():
            snapshot = encode_workspace({'roots': self.roots, 'default_root': 'other'})
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path('local_files_mcp.py').resolve()), '--workspace-settings', snapshot])
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    await session.initialize()
                    tools = (await session.list_tools()).tools
                    self.assertEqual({tool.name for tool in tools}, set(TOOLS))
                    for tool in tools:
                        self.assertTrue(tool.annotations.readOnlyHint)
                        self.assertFalse(tool.annotations.destructiveHint)
                        self.assertTrue(tool.annotations.idempotentHint)
                        self.assertFalse(tool.annotations.openWorldHint)
                        if tool.name != 'workspace_info':
                            self.assertIn('root_id', tool.inputSchema['properties'])
                    for name, args in [('workspace_info', {}), ('list_projects', {}), ('project_context', {}),
                                       ('list_directory', {}), ('list_files', {}),
                                       ('read_file', {'path': 'README.md'}),
                                       ('read_files', {'files': [{'path': 'README.md'}]}),
                                       ('search_text', {'query': 'beta', 'context_lines': 3})]:
                        result = await session.call_tool(name, args)
                        self.assertFalse(result.isError, str(result))
                        self.assertNotIn(str(self.base), str(result.content))
                        if name in ('read_file', 'read_files'):
                            self.assertIn('beta', str(result.content))
                    denied = await session.call_tool('read_file', {'path': 'missing.txt'})
                    self.assertTrue(denied.isError)
                    self.assertNotIn(str(self.base), str(denied.content))
                    for index in range(10):
                        (self.b / f'{index}.txt').write_text('中' * 23000, encoding='utf-8')
                    batch = await session.call_tool('read_files', {
                        'files': [{'path': f'{i}.txt'} for i in range(10)]})
                    self.assertFalse(batch.isError)
                    self.assertLessEqual(len(batch.content[0].text.encode('utf-8')), BATCH_BYTES)
                    self.assertTrue(json.loads(batch.content[0].text)['truncated'])
        asyncio.run(smoke())

    def test_new_tools_share_hidden_link_and_capacity_guards(self):
        import ctypes
        import subprocess
        folder = self.a / 'project'
        folder.mkdir()
        entry = folder / 'README.md'
        entry.write_text('secret-marker', encoding='utf-8')
        def check_blocked():
            self.assertEqual(self.reader.list_projects()['projects'][0]['entry_files'], [])
            self.assertNotIn('secret-marker', str(self.reader.project_context('project')))
            self.assertNotIn('secret-marker', str(self.reader.read_files([{'path': 'project/README.md'}])))
        self.assertTrue(ctypes.windll.kernel32.SetFileAttributesW(str(entry), 2))
        try:
            check_blocked()
        finally:
            ctypes.windll.kernel32.SetFileAttributesW(str(entry), 128)
        os.link(entry, folder / 'hard.txt')
        check_blocked()
        (folder / 'hard.txt').unlink()
        entry.write_bytes(b'bad\x00secret-marker')
        self.assertNotIn('secret-marker', str(self.reader.project_context('project')))
        entry.write_bytes(b'\xffsecret-marker')
        self.assertNotIn('secret-marker', str(self.reader.project_context('project')))
        entry.write_text('secret-marker', encoding='utf-8')
        small = WorkspaceReader(self.roots, 'main', {'max_file_bytes': 3})
        self.assertNotIn('secret-marker', str(small.project_context('project')))
        junction = self.a / 'junction'
        result = subprocess.run(['cmd', '/c', 'mklink', '/J', str(junction), str(self.b)],
                                capture_output=True, creationflags=subprocess.CREATE_NO_WINDOW)
        self.assertEqual(result.returncode, 0)
        try:
            self.assertNotIn('junction', str(self.reader.list_projects()))
            with self.assertRaises(ValueError):
                self.reader.project_context('junction')
            self.assertIn('skipped_reason', self.reader.read_files([{'path': 'junction/README.md'}])['files'][0])
        finally:
            junction.rmdir()
        link = folder / 'AGENTS.md'
        try:
            link.symlink_to(self.b / 'README.md')
        except OSError:
            pass  # 獨立 symlink 測試記錄平台跳過；junction 已實測。
        else:
            try:
                self.assertNotIn('AGENTS.md', self.reader.list_projects()['projects'][0]['entry_files'])
                self.assertNotIn('beta', str(self.reader.project_context('project')))
            finally:
                link.unlink()
        self.assertTrue(ctypes.windll.kernel32.SetFileAttributesW(str(self.a), 2))
        try:
            for method, args in [('workspace_info', ()), ('list_projects', ()), ('project_context', ()),
                                 ('read_files', ([{'path': 'README.md'}],))]:
                with self.subTest(method=method), self.assertRaises(ValueError):
                    getattr(self.reader, method)(*args)
        finally:
            ctypes.windll.kernel32.SetFileAttributesW(str(self.a), 128)

    def test_symlink_entry(self):
        link = self.a / 'AGENTS.md'
        try:
            link.symlink_to(self.b / 'README.md')
        except OSError as exc:
            self.skipTest('Windows 未允許建立符號連結：' + str(exc.winerror))
        self.addCleanup(link.unlink)
        self.assertNotIn('beta', str(self.reader.project_context()))
        self.assertIn('skipped_reason', self.reader.read_files([{'path': 'AGENTS.md'}])['files'][0])

    def test_command_snapshot_and_gui_persistence(self):
        import tkinter as tk
        from unittest.mock import Mock
        from local_files_gui import App
        from workspace_settings import decode_workspace
        settings = normalize_connection({'roots': self.roots, 'default_root': 'main',
                                         'tunnel': 'tunnel_test', 'settings_version': 2})
        commands = build_commands(settings)
        command = commands[0][commands[0].index('--mcp-command') + 1]
        decoded = decode_workspace(command.split(' --workspace-settings ')[1].split(' ')[0])
        self.assertEqual(len(decoded['roots']), 2)
        self.assertNotIn('key', decoded)
        window = tk.Tk()
        window.withdraw()
        store = Mock()
        store.load.return_value = ''
        with patch('tray_windows.Tray'):
            app = App(window, settings, store)
        try:
            app.folder_list.selection_set(0)
            app.remove_selected_folders()
            self.assertEqual(app.workspace_rows[0]['path'], str(self.b))
            target = self.base / 'settings.json'
            with patch('local_files_gui.save_settings', side_effect=lambda value: save_settings(value, target)):
                self.assertTrue(app.save())
                import time
                deadline = time.monotonic() + 5
                while app.tasks.busy and time.monotonic() < deadline:
                    app.tasks.poll()
                    time.sleep(.01)
                self.assertFalse(app.tasks.busy)
            loaded = load_settings(target)
            self.assertEqual(loaded['default_root'], 'other')
            self.assertEqual(loaded['root'], str(self.b))
            self.assertEqual(len(loaded['roots']), 1)
            self.assertEqual(loaded['roots'][0]['excluded_names'], ['private.txt'])
        finally:
            app.quit()

    def test_exclusion_union_validated_before_save(self):
        settings = {'excluded_names': [f'exclude{i}' for i in range(128)]}
        roots = [dict(self.roots[0], excluded_names=['extra'])]
        with self.assertRaises(ValueError):
            normalize_workspace(roots, 'main', settings)
        accepted = WorkspaceReader([self.roots[0]], 'main', settings)
        self.assertIn('credentials.json', accepted.workspace_info()['roots'][0]['limits']['excluded_names'])

    def test_snapshot_respects_custom_global_exclusions(self):
        from workspace_settings import decode_workspace
        roots = [dict(self.roots[0], excluded_names=[f'exclude{i}' for i in range(128)])]
        settings = {'excluded_names': []}
        snapshot = encode_workspace({'roots': roots, 'default_root': 'main', 'reader': settings})
        decoded = decode_workspace(snapshot, settings)
        self.assertEqual(len(decoded['roots'][0]['excluded_names']), 128)
        WorkspaceReader(decoded['roots'], decoded['default_root'], settings)
