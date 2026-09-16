"""設定保存與既有唯讀邊界的聚焦回歸測試。"""
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from local_files_gui import build_commands, load_settings, save_settings, validate_settings
from local_files_mcp import FileReader


class SettingsTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def test_save_reload_excludes_secret_and_remembers_recent(self):
        settings = validate_settings(str(self.root), 'tunnel_test')
        settings.update(recent=[str(self.root)], start_hidden=True, key='not-a-real-key')
        target = self.root / 'settings.json'
        save_settings(settings, target)
        self.assertNotIn('key', json.loads(target.read_text(encoding='utf-8')))
        self.assertEqual(load_settings(target)['recent'], [str(self.root)])
        self.assertTrue(load_settings(target)['start_hidden'])

    def test_invalid_settings_preserve_original(self):
        target = self.root / 'settings.json'
        target.write_text('{broken', encoding='utf-8')
        with self.assertRaises(ValueError):
            load_settings(target)
        self.assertEqual(target.read_text(), '{broken')
        with self.assertRaises(ValueError):
            validate_settings(str(self.root), 'invalid --other-flag')

    def test_unsafe_root_rejected(self):
        for root in (Path(self.root.anchor), Path.home(), self.root / 'missing'):
            with self.subTest(root=root), self.assertRaises(ValueError):
                validate_settings(str(root), 'tunnel_test')

    def test_command_preserves_spaces_and_shell_characters(self):
        folder = self.root / '中文 空格 & example'
        folder.mkdir()
        commands = build_commands(validate_settings(str(folder), 'tunnel_test'))
        command = commands[0][commands[0].index('--mcp-command') + 1]
        self.assertIn('--root "' + str(folder.resolve()).replace('\\', '/') + '"', command)
        self.assertNotIn('CONTROL_PLANE_API_KEY', ' '.join(commands[0]))

    def test_all_three_tools_and_text_boundaries(self):
        (self.root / 'sample.md').write_text('測試\nHello marker\n', encoding='utf-8')
        (self.root / '.hidden.md').write_text('secret', encoding='utf-8')
        (self.root / 'bad.txt').write_bytes(b'\xff\xfe')
        (self.root / 'image.png').write_bytes(b'image')
        reader = FileReader(self.root)
        self.assertIn('sample.md', reader.list_files()['files'])
        self.assertNotIn('.hidden.md', reader.list_files()['files'])
        self.assertEqual(reader.read_file('sample.md', 2, 1)['content'], '2: Hello marker')
        self.assertEqual(reader.search_text('MARKER')['matches'][0]['path'], 'sample.md')
        for path in ('../outside.txt', '.hidden.md', 'bad.txt', 'image.png'):
            with self.subTest(path=path), self.assertRaises(ValueError):
                reader.read_file(path)

    def test_size_scan_limits_and_hardlinks(self):
        file = self.root / 'sample.txt'
        file.write_text('abcdef', encoding='utf-8')
        reader = FileReader(self.root, {'max_file_bytes': 3})
        with self.assertRaises(ValueError):
            reader.read_file('sample.txt')
        (self.root / 'second.txt').write_text('abc', encoding='utf-8')
        self.assertTrue(FileReader(self.root, {'max_scan_entries': 1}).list_files()['truncated'])
        self.assertTrue(FileReader(self.root, {'max_scan_files': 1}).list_files()['truncated'])
        self.assertTrue(FileReader(self.root, {'max_scan_bytes': 1}).search_text('a')['truncated'])
        os.link(file, self.root / 'linked.txt')
        with self.assertRaises(ValueError):
            reader.read_file('linked.txt')


if __name__ == '__main__':
    unittest.main()
