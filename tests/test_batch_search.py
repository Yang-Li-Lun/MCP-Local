import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from local_files_mcp import FileReader


class BatchSearchTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.reader = FileReader(self.root)

    def test_single_walk_and_open_with_equivalent_context(self):
        (self.root / 'a.txt').write_text('before\nStraße alpha\nafter\nbeta', encoding='utf-8')
        expected = [self.reader.search_text(q, context_lines=1)['matches'] for q in ['STRASSE', 'beta']]
        with patch.object(self.reader, 'walk', wraps=self.reader.walk) as walk, patch.object(
                self.reader, 'open_checked', wraps=self.reader.open_checked) as opened:
            result = self.reader.search_texts(['STRASSE', 'beta'], context_lines=1)
        self.assertEqual(walk.call_count, 1)
        self.assertEqual(opened.call_count, 1)
        self.assertEqual([g['matches'] for g in result['results']], expected)

    def test_invalid_tail_discards_all_hits(self):
        for tail in [b'\x00', b'\xff', b'\xe4']:
            (self.root / 'a.txt').write_bytes(b'alpha beta\n' + b'x' * 70000 + tail)
            result = self.reader.search_texts(['alpha', 'beta'])
            self.assertTrue(all(not g['matches'] for g in result['results']))
            self.assertEqual(result['skipped_entries'], 1)

    def test_chunk_boundary_and_limits(self):
        (self.root / 'a.txt').write_text('x' * 65534 + 'abcdef\n' + 'abcdef\n' * 120, encoding='utf-8')
        result = self.reader.search_texts(['abcdef'] * 10, limit_per_query=100)
        self.assertLessEqual(sum(len(g['matches']) for g in result['results']), 200)
        self.assertTrue(result['truncated'])
        self.assertEqual(result['results'][0]['matches'][0]['line'], 1)
        self.assertIn('abcdef', result['results'][0]['matches'][0]['match_snippet'])

    def test_reject_invalid_arguments_and_escape(self):
        for queries in [[], ['a'] * 11, [''], ['\n'], [1], 'abc']:
            with self.assertRaises(ValueError):
                self.reader.search_texts(queries)
        with self.assertRaises(ValueError):
            self.reader.search_texts(['a'], directory='..')
        with self.assertRaises(ValueError):
            self.reader.search_texts(['a'], limit_per_query=True)

    def test_scan_budget(self):
        (self.root / 'a.txt').write_bytes(b'alpha\n' * 100)
        self.reader.settings['max_scan_bytes'] = 20
        result = self.reader.search_texts(['alpha'])
        self.assertLessEqual(result['scanned_bytes'], 20)
        self.assertFalse(result['results'][0]['matches'])
        self.assertTrue(result['truncated'])
