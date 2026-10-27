"""Regression checks for the archived benchmark module source."""
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools.benchmarks import benchmark_actual_performance as benchmark


class BenchmarkSnapshotTests(unittest.TestCase):
    def test_working_directory_cannot_select_archive_module(self):
        with tempfile.TemporaryDirectory() as temporary:
            project = Path(temporary) / 'project'
            script = project / 'tools/benchmarks/benchmark_actual_performance.py'
            expected = (project / 'archive/backups'
                        / 'performance-backup-20260913-180807/local_files_mcp.py')
            expected.parent.mkdir(parents=True)
            expected.write_text("SENTINEL = 'expected snapshot'", encoding='utf-8')
            decoy = (Path(temporary) / 'archive/backups'
                     / 'performance-backup-20260913-180807/local_files_mcp.py')
            decoy.parent.mkdir(parents=True)
            decoy.write_text('raise AssertionError("decoy executed")', encoding='utf-8')
            previous = Path.cwd()
            try:
                os.chdir(temporary)
                with patch.object(benchmark, '__file__', str(script)):
                    module = benchmark.load_original_module()
            finally:
                os.chdir(previous)
            self.assertEqual(Path(module.__file__).resolve(), expected)
            self.assertNotEqual(expected, decoy)
            self.assertEqual(module.SENTINEL, 'expected snapshot')

    def test_missing_snapshot_fails_before_module_execution(self):
        with tempfile.TemporaryDirectory() as temporary:
            script = Path(temporary) / 'tools/benchmarks/benchmark_actual_performance.py'
            with patch.object(benchmark, '__file__', str(script)), patch.object(
                    benchmark.importlib.util, 'spec_from_file_location') as select:
                with self.assertRaises(FileNotFoundError):
                    benchmark.load_original_module()
            select.assert_not_called()

    def test_snapshot_link_outside_repository_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            script = root / 'tools/benchmarks/benchmark_actual_performance.py'
            snapshot = (root / 'archive/backups'
                        / 'performance-backup-20260913-180807/local_files_mcp.py')
            snapshot.parent.mkdir(parents=True)
            outside = root.parent / (root.name + '-outside.py')
            outside.write_text('pass', encoding='utf-8')
            self.addCleanup(outside.unlink, missing_ok=True)
            try:
                snapshot.symlink_to(outside)
            except OSError as exc:
                self.skipTest(f'Cannot create a file link: {exc}')
            with patch.object(benchmark, '__file__', str(script)), patch.object(
                    benchmark.importlib.util, 'spec_from_file_location') as select:
                with self.assertRaises(ValueError):
                    benchmark.load_original_module()
            select.assert_not_called()
