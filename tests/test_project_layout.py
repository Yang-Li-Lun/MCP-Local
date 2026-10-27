"""部署佈局、相容入口與工作目錄獨立性。"""
import importlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from tools.build_release import PAYLOAD, ROOT
from mcp_local.connection_settings import PROJECT
from mcp_local.icon_assets import icon_path


class ProjectLayoutTests(unittest.TestCase):
    def test_legacy_entries_share_the_canonical_module(self):
        for name in ('local_files_mcp', 'local_files_gui', 'connection_cli', 'autostart', 'access_mode'):
            self.assertIs(importlib.import_module(name), importlib.import_module('mcp_local.' + name))
        self.assertEqual(PROJECT, ROOT)

    def test_payload_covers_runtime_and_fixed_resources(self):
        self.assertEqual(len(PAYLOAD), len(set(PAYLOAD)))
        self.assertEqual([name for name in PAYLOAD if not (ROOT / name).is_file()], [])
        for path in (ROOT / 'mcp_local').glob('*'):
            if path.suffix in ('.py', '.ps1'):
                self.assertIn(path.relative_to(ROOT).as_posix(), PAYLOAD)
        for name in ('tool-contract.json', 'tool-contract-full-control.json',
                     'tool-contract-developer-control.json', 'tool-contract-host-control.json'):
            self.assertIn('contracts/' + name, PAYLOAD)
        for style in ('classic', 'folder_link'):
            for preview in (False, True):
                self.assertIn(icon_path(style, preview=preview).relative_to(ROOT).as_posix(), PAYLOAD)

    def test_entrypoints_work_from_an_unrelated_directory(self):
        with tempfile.TemporaryDirectory(prefix='mcp-entry-') as folder:
            for name in ('local_files_mcp.py', 'connection_cli.py',
                         'tools/verification/verify_access_modes_stdio.py', 'build_release.py'):
                result = subprocess.run([sys.executable, '-B', str(ROOT / name), '--help'],
                                        cwd=folder, capture_output=True, timeout=30)
                self.assertEqual(result.returncode, 0, (name, result.stderr))

    def test_isolated_worker_resolves_package_from_an_unrelated_directory(self):
        with tempfile.TemporaryDirectory(prefix='mcp-worker-layout-') as folder:
            result = subprocess.run([sys.executable, '-I', '-B', str(ROOT / 'mcp_local/format_worker.py')],
                                    input=b'{}', cwd=folder, capture_output=True, timeout=30)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('error', json.loads(result.stdout))
