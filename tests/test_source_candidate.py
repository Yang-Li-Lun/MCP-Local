"""完整來源候選的精確集合、可重現性與排除邊界。"""
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from tools import build_source_candidate as candidate
from tools.build_release import MANIFEST, PAYLOAD, ROOT


class SourceCandidateTests(unittest.TestCase):
    def test_reproducible_archive_contains_exact_reviewed_sources(self):
        names = json.loads((ROOT / candidate.SOURCE_LIST).read_text(encoding='utf-8'))['files']
        with tempfile.TemporaryDirectory(prefix='mcp-source-candidate-') as temporary:
            first = candidate.build(Path(temporary) / 'first')
            second = candidate.build(Path(temporary) / 'second')
            self.assertEqual(first['sha256'], second['sha256'])
            self.assertFalse(first['clean_clone'])
            with zipfile.ZipFile(Path(temporary) / 'first' / first['package']) as archive:
                self.assertEqual(set(archive.namelist()), set(names) | {MANIFEST})
                for name in names:
                    self.assertEqual(archive.read(name), (ROOT / name).read_bytes())
            with self.assertRaises(FileExistsError):
                candidate.build(Path(temporary) / 'first')

    def test_missing_deployment_dependency_is_rejected(self):
        with patch.object(candidate, 'source_bytes', return_value=json.dumps(
                {'files': [candidate.SOURCE_LIST]}).encode()):
            with self.assertRaises(ValueError):
                candidate.build(Path('must-not-be-created'))

    def test_private_or_unsafe_paths_are_rejected_before_read(self):
        for private in ('../outside.py', 'reports/private.txt', '.venv/pyvenv.cfg',
                        'archive/backups/old.py', 'keys/api-key.dpapi', 'settings.json.bak'):
            names = sorted(set(PAYLOAD) | {candidate.SOURCE_LIST, private})
            with self.subTest(path=private), patch.object(candidate, 'source_bytes',
                    return_value=json.dumps({'files': names}).encode()) as read:
                with self.assertRaises(ValueError):
                    candidate.build(Path('must-not-be-created'))
                read.assert_called_once_with(candidate.SOURCE_LIST)
