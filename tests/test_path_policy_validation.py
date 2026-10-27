"""完整候選與解析後政策驗證，保留每段即時屬性及全文讀取檢查。"""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from mcp_local.local_files_mcp import FileReader
from mcp_local import security_policy


class PathPolicyValidationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-path-policy-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / 'root'
        self.target = self.root / 'one/two/file.txt'
        self.target.parent.mkdir(parents=True)
        self.target.write_bytes(b'first\nsecond\n')
        self.reader = FileReader(self.root)

    def test_each_ancestor_and_leaf_protected_before_traversal(self):
        original = Path.lstat
        for protected in (self.root, self.root / 'one', self.target.parent, self.target):
            visited = []

            def observed(path, *args, **kwargs):
                visited.append(path)
                return original(path, *args, **kwargs)

            with self.subTest(protected=protected), patch.object(security_policy, 'STATE_DIR', protected):
                with patch.object(Path, 'lstat', observed), self.assertRaisesRegex(ValueError, '設定、備份及金鑰'):
                    self.reader.checked('one/two/file.txt')
            self.assertNotIn(self.root / 'one', visited)

    def test_policy_changes_during_traversal_and_between_calls(self):
        original = Path.lstat

        def changed(path, *args, **kwargs):
            if path == self.target.parent:
                security_policy.STATE_DIR = self.root / 'one'
            return original(path, *args, **kwargs)

        with patch.object(security_policy, 'STATE_DIR', self.base / 'elsewhere'):
            self.assertEqual(self.reader.checked('one/two/file.txt'), self.target)
            with patch.object(Path, 'lstat', changed), self.assertRaisesRegex(ValueError, '設定、備份及金鑰'):
                self.reader.checked('one/two/file.txt')
            with self.assertRaises(ValueError):
                self.reader.checked('one/two/file.txt')

    def test_alias_policy_and_neighbor_names(self):
        alias = self.base / 'state-alias'
        alias.symlink_to(self.target.parent, target_is_directory=True)
        with patch.object(security_policy, 'STATE_DIR', alias), self.assertRaises(ValueError):
            self.reader.checked('one/two/file.txt')
        with patch.object(security_policy, 'STATE_DIR', self.root / 'on'):
            self.assertEqual(self.reader.checked('one/two/file.txt'), self.target)

    def test_every_ancestor_is_still_statted_and_dynamic_exclusions_apply(self):
        original = Path.lstat
        visited = []

        def observed(path, *args, **kwargs):
            visited.append(path)
            return original(path, *args, **kwargs)

        with patch.object(Path, 'lstat', observed):
            self.reader.checked('one/two/file.txt')
        for path in (self.root, self.root / 'one', self.target.parent, self.target):
            self.assertIn(path, visited)
        self.reader.settings['excluded_names'].append('two')
        with self.assertRaises(ValueError):
            self.reader.checked('one/two/file.txt')

    def test_no_reuse_of_content_even_with_same_size_and_mtime(self):
        info = self.target.stat()
        self.assertIn('first', self.reader.read_file('one/two/file.txt', line_count=1)['content'])
        import os
        self.target.write_bytes(b'first\nsecon\xff\n')
        os.utime(self.target, ns=(info.st_atime_ns, info.st_mtime_ns))
        with self.assertRaisesRegex(ValueError, 'UTF-8'):
            self.reader.read_file('one/two/file.txt', line_count=1)
