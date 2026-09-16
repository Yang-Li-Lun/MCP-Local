"""分頁、安全與實際讀取預算的非敏感固定資料測試。"""
import base64
import ctypes
import hmac
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from local_files_mcp import FileReader


class PaginationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def all_pages(self, reader, limit):
        cursor = None
        files = []
        while True:
            result = reader.list_files(limit=limit, cursor=cursor)
            self.assertLessEqual(result['returned_count'], limit)
            self.assertEqual(result['truncated'], result['has_more'] or result['scan_truncated'])
            files.extend(result['files'])
            if not result['has_more']:
                self.assertIsNone(result['next_cursor'])
                return files, result
            self.assertTrue(result['files'])
            self.assertNotEqual(cursor, result['next_cursor'])
            cursor = result['next_cursor']

    def test_scale_boundaries_and_3000_complete(self):
        count = 0
        for target in (0, 1, 200, 201, 500, 501, 3000, 3001):
            for index in range(count, target):
                (self.root / f'{index:04}.txt').write_text('marker', encoding='utf-8')
            count = target
            for limit in (200, 500):
                with self.subTest(target=target, limit=limit):
                    files, result = self.all_pages(FileReader(self.root), limit)
                    self.assertEqual(len(files), min(target, 3000))
                    self.assertEqual(len(set(files)), len(files))
                    self.assertEqual(files, sorted(files, key=lambda p: (p.casefold(), p)))
                    self.assertEqual(result['scan_truncated'], target > 3000)

    def test_cursor_validation_and_deleted_anchor(self):
        for name in ('Alpha.txt', 'alpha2.txt', '中文.txt', '中文前綴.txt'):
            (self.root / name).touch()
        reader = FileReader(self.root)
        first = reader.list_files(limit=1)
        cursor = first['next_cursor']
        self.assertEqual(first['files'], reader.list_files(limit=1)['files'])
        (self.root / 'sub').mkdir()
        for call in (
            lambda: reader.list_directory(cursor=cursor),
            lambda: reader.list_files('sub', cursor=cursor),
            lambda: reader.list_files(cursor='bad'),
            lambda: reader.list_files(cursor=''),
            lambda: reader.list_files(cursor=cursor[:-4] + 'AAAA'),
            lambda: FileReader(self.root).list_files(cursor=cursor),
        ):
            with self.assertRaises(ValueError):
                call()
        data = json.dumps([2, 'list_files', '.', 'Alpha.txt']).encode()
        unsupported = base64.urlsafe_b64encode(hmac.digest(reader._cursor_key, data, 'sha256') + data).decode()
        with self.assertRaisesRegex(ValueError, '版本'):
            reader.list_files(cursor=unsupported)
        (self.root / first['files'][0]).unlink()
        self.assertEqual(len(reader.list_files(cursor=cursor)['files']), 3)
        for limit in (0, 501, True, 1.5):
            with self.assertRaises(ValueError):
                reader.list_files(limit=limit)

    def test_shallow_navigation_and_scan_flags(self):
        (self.root / 'sub').mkdir()
        (self.root / 'sub' / 'deep.txt').touch()
        (self.root / 'a.txt').touch()
        (self.root / 'image.png').touch()
        reader = FileReader(self.root)
        page = reader.list_directory(limit=1)
        second = reader.list_directory(cursor=page['next_cursor'])
        self.assertEqual([entry['path'] for entry in page['entries'] + second['entries']], ['a.txt', 'sub'])
        self.assertFalse(second['has_more'])
        self.assertEqual(second['entries'][0]['type'], 'directory')
        limited = FileReader(self.root, {'max_scan_entries': 1}).list_files()
        self.assertTrue(limited['scan_truncated'])
        self.assertFalse(limited['has_more'])
        with self.assertRaises(ValueError):
            reader.list_directory('a.txt')

    def test_paths_links_and_disappearing_entry(self):
        file = self.root / 'safe.txt'
        file.touch()
        (self.root / '.hidden.txt').touch()
        reader = FileReader(self.root)
        for path in ('../x', '/x', 'C:\\x', 'x\x00.txt', '.hidden.txt'):
            with self.assertRaises(ValueError):
                reader.checked(path)
        os.link(file, self.root / 'hard.txt')
        self.assertEqual(reader.list_files()['files'], [])
        (self.root / 'hard.txt').unlink()
        original = reader._entry_info
        def disappear(folder, entry):
            if entry.name == 'safe.txt':
                file.unlink()
                raise FileNotFoundError()
            return original(folder, entry)
        with patch.object(reader, '_entry_info', side_effect=disappear):
            page = reader.list_files()
        self.assertEqual(page['files'], [])
        self.assertGreaterEqual(page['skipped_entries'], 2)

    @unittest.skipUnless(os.name == 'nt', '需要 Windows Hidden 屬性')
    def test_windows_hidden_file_folder_root_and_attribute_failure(self):
        file = self.root / 'ordinary.txt'
        file.write_text('marker')
        folder = self.root / 'folder'
        folder.mkdir()
        (folder / 'nested.txt').write_text('marker')
        set_attributes = ctypes.windll.kernel32.SetFileAttributesW
        for target in (file, folder):
            self.assertTrue(set_attributes(str(target), 2))
        try:
            reader = FileReader(self.root, {'excluded_names': []})
            self.assertEqual(reader.list_files()['files'], [])
            self.assertEqual(reader.search_text('marker')['matches'], [])
            for name in ('ordinary.txt', 'folder/nested.txt'):
                with self.assertRaises(ValueError):
                    reader.read_file(name)
            self.assertTrue(set_attributes(str(self.root), 2))
            try:
                with self.assertRaises(ValueError):
                    FileReader(self.root)
            finally:
                set_attributes(str(self.root), 128)
        finally:
            for target in (file, folder):
                set_attributes(str(target), 128)
        self.assertEqual(len(reader.list_files()['files']), 2)
        with patch('local_files_mcp.hidden', side_effect=OSError('unavailable')):
            with self.assertRaises(OSError):
                reader.checked('ordinary.txt')

    def test_symbolic_link(self):
        (self.root / 'source.txt').touch()
        try:
            (self.root / 'symbol.txt').symlink_to(self.root / 'source.txt')
        except OSError as exc:
            self.skipTest(f'環境不允許建立符號連結：{exc.winerror if os.name == "nt" else exc.errno}')
        with self.assertRaises(ValueError):
            FileReader(self.root).read_file('symbol.txt')

    @unittest.skipUnless(os.name == 'nt', '需要 Windows junction')
    def test_junction(self):
        folder = self.root / 'target'
        folder.mkdir()
        junction = self.root / 'junction'
        result = subprocess.run(['cmd', '/c', 'mklink', '/J', str(junction), str(folder)], capture_output=True)
        self.assertEqual(result.returncode, 0)
        try:
            with self.assertRaises(ValueError):
                FileReader(self.root).checked('junction')
        finally:
            junction.rmdir()

    def test_invalid_utf8_nul_and_exact_budget(self):
        for index, data in enumerate((b'\xff' * 4, b'\0' * 4, b'marker')):
            (self.root / f'{index}.txt').write_bytes(data)
        result = FileReader(self.root, {'max_scan_bytes': 8}).search_text('marker')
        self.assertEqual(result['scanned_bytes'], 8)
        self.assertEqual(result['scanned_files'], 0)
        self.assertEqual(result['skipped_entries'], 2)
        self.assertTrue(result['truncated'])
        self.assertFalse(result['matches'])
        for item in self.root.iterdir():
            item.unlink()
        (self.root / '0.txt').write_bytes(b'')
        (self.root / '1.txt').write_bytes(b'abc')
        reader = FileReader(self.root, {'max_scan_bytes': 3})
        result = reader.search_text('abc')
        self.assertEqual(result['scanned_files'], 2)
        self.assertEqual(result['scanned_bytes'], 3)
        self.assertEqual(len(result['matches']), 1)
        self.assertTrue(result['truncated'])
        (self.root / '1.txt').write_bytes(b'abcdef')
        self.assertFalse(reader.search_text('abc')['matches'])

    def test_growth_after_stat_and_bom(self):
        file = self.root / 'test.txt'
        file.write_bytes(b'abc')
        file = file.resolve()
        reader = FileReader(self.root, {'max_scan_bytes': 4})
        original = Path.open
        def grow(path, *args, **kwargs):
            if path == file and args == ('rb',):
                with original(file, 'wb') as handle:
                    handle.write(b'abcdef')
            return original(path, *args, **kwargs)
        with patch.object(Path, 'open', grow):
            result = reader.search_text('abc')
        self.assertEqual(result['scanned_bytes'], 4)
        self.assertFalse(result['matches'])
        file.write_bytes(b'\xef\xbb\xbfhello\nworld')
        self.assertEqual(reader.read_file('test.txt', 2, 1)['content'], '2: world')
