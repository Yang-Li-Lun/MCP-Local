"""DirEntry 快速列舉的安全邊界回歸。"""
import os
from pathlib import Path
import stat
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
from local_files_mcp import FileReader


class PerformanceSafetyTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.reader = FileReader(self.root)
        self.root = self.reader.root

    def test_entry_attributes_fail_closed_and_single_no_follow_stat(self):
        for mode, links, attributes in (
                (stat.S_IFREG, 1, 0), (stat.S_IFREG, 2, 0),
                (stat.S_IFLNK, 1, 0), (stat.S_IFREG, 1, 2),
                (stat.S_IFDIR, 1, 0x400)):
            entry = Mock()
            entry.name = 'safe.txt'
            entry.stat.return_value = SimpleNamespace(
                st_mode=mode, st_nlink=links, st_file_attributes=attributes)
            if (mode, links, attributes) == (stat.S_IFREG, 1, 0):
                self.reader._entry_info(self.root, entry)
            else:
                with self.assertRaises(ValueError):
                    self.reader._entry_info(self.root, entry)
            entry.stat.assert_called_once_with(follow_symlinks=False)
        entry.stat.side_effect = OSError('denied')
        with self.assertRaises(OSError):
            self.reader._entry_info(self.root, entry)

    def test_unsafe_names_rejected_before_metadata(self):
        for name in ('.secret', 'build', 'secrets.json', 'CON.txt', 'a.',
                     'a ', 'a:b', '../x', 'a/b', 'a*', 'a' + chr(0)):
            entry = Mock()
            entry.name = name
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.reader._entry_info(self.root, entry)
            entry.stat.assert_not_called()

    def test_nested_listing_and_pending_directory_revalidation(self):
        child = self.root / 'child'
        child.mkdir()
        (child / 'safe.txt').write_text('marker')
        self.assertEqual(self.reader.list_files()['files'], ['child/safe.txt'])
        original = self.reader.checked
        def denied(relative):
            if relative == 'child':
                raise ValueError('directory changed')
            return original(relative)
        with patch.object(self.reader, 'checked', side_effect=denied):
            result = self.reader.list_files()
        self.assertEqual(result['files'], [])
        self.assertEqual(result['skipped_entries'], 1)

    def test_snapshot_does_not_authorize_replaced_hardlink(self):
        file = self.root / 'safe.txt'
        file.write_text('marker')
        self.assertEqual(self.reader.list_files()['files'], ['safe.txt'])
        other = self.root / 'other.txt'
        other.write_text('secret')
        file.unlink()
        os.link(other, file)
        with self.assertRaises(ValueError):
            self.reader.read_file('safe.txt')
        self.assertEqual(self.reader.search_text('secret')['matches'], [])

    def test_first_page_does_not_recheck_each_file(self):
        for i in range(20):
            (self.root / f'{i}.txt').touch()
        with patch.object(self.reader, 'checked', wraps=self.reader.checked) as checked:
            self.assertEqual(self.reader.list_files()['sorted_entries'], 20)
        self.assertLess(checked.call_count, 10)

    def test_search_revalidates_file_after_enumeration(self):
        file = self.root / 'safe.txt'
        file.write_text('marker')
        walk = self.reader.walk
        def changed(*args, **kwargs):
            for path in walk(*args, **kwargs):
                file.unlink()
                yield path
        with patch.object(self.reader, 'walk', side_effect=changed):
            self.assertEqual(self.reader.search_text('marker')['matches'], [])

    def test_zero_link_count_requires_fresh_metadata(self):
        file = self.root / 'safe.txt'
        file.touch()
        os.link(file, self.root / 'hard.txt')
        entry = Mock()
        entry.name = 'safe.txt'
        entry.stat.return_value = SimpleNamespace(
            st_mode=stat.S_IFREG, st_nlink=0, st_file_attributes=0)
        with self.assertRaises(ValueError):
            self.reader._entry_info(self.root, entry)
