"""桌面查詢去重、執行身分及 runtime 提示；只有合成資料。"""
from collections import OrderedDict
import hashlib
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from mcp_local.desktop_bridge import DesktopApi
from mcp_local import desktop_identity, desktop_launcher
from mcp_local.request_monitor import Monitor


class DesktopOptimizationTests(unittest.TestCase):
    def test_monitor_reuses_unchanged_summaries_and_invalidates_every_status(self):
        monitor = Monitor()
        monitor.records = OrderedDict((str(i), dict(id=str(i), _revision=i, time='now', tool='read_file', status='running')) for i in range(3))
        api = DesktopApi(SimpleNamespace(request_monitor=monitor))
        with patch('mcp_local.request_monitor_presenter.argument_summary', return_value='synthetic') as summary:
            first = api.monitor()
            self.assertEqual(summary.call_count, 3)
            self.assertEqual(api.monitor(first['version']), {'unchanged': True, 'version': first['version']})
            self.assertIs(api.monitor(), first)
            self.assertEqual(summary.call_count, 3)
            monitor.records['1'].update(_revision=9, status='success')
            second = api.monitor(first['version'])
            self.assertEqual(summary.call_count, 4)
            self.assertIs(second['records'][0], first['records'][0])
            for field in ('enabled', 'connected', 'attached', 'failed', 'lost_events', 'evicted_records', 'dropped', 'used'):
                previous = api.monitor()['version']
                value = getattr(monitor, field)
                setattr(monitor, field, not value if isinstance(value, bool) else value + 1)
                self.assertGreater(api.monitor()['version'], previous, field)
            self.assertEqual(summary.call_count, 4)
            monitor.records.pop('0')
            self.assertEqual(len(api.monitor()['records']), 2)
            monitor.records.clear()
            self.assertEqual(api.monitor()['records'], [])
            self.assertFalse(api._monitor_rows)

    def test_identity_distinguishes_executed_bytes_from_replaced_disk(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            module = root / 'mcp_local/fixture.py'
            module.parent.mkdir()
            data = b'VALUE = 1\n'
            module.write_bytes(data)
            expected = {'mcp_local/fixture.py': hashlib.sha256(data).hexdigest()}
            with patch.object(desktop_identity, 'ROOT', root), patch.object(desktop_identity, 'LOADED', {}), \
                 patch.object(desktop_identity, 'MANIFEST', {'backend_sources': expected, 'backend_build_id': 'fixture-build', 'source': {'commit': 'fixture'}}):
                code = desktop_identity.SourceLoader('fixture', str(module)).get_code('fixture')
                scope = {}
                exec(code, scope)
                self.assertEqual(scope['VALUE'], 1)
                first = desktop_identity.diagnostics()
                self.assertTrue(first['loaded_matches_build'])
                module.write_text('VALUE = 2\n', encoding='utf-8')
                second = desktop_identity.diagnostics()
                self.assertEqual(second['loaded_code_sha256'], first['loaded_code_sha256'])
                self.assertEqual(second['backend_build_id'], 'fixture-build')
                self.assertFalse(second['disk_matches_loaded_now'])
                self.assertFalse(second['disk_matches_build_now'])
                self.assertEqual(second['process_start_id'], first['process_start_id'])
                desktop_identity.SourceLoader('fixture', str(module)).get_code('fixture')
                self.assertEqual(desktop_identity.diagnostics()['backend_build_id'], '未知')

    def test_missing_metadata_is_unknown(self):
        with patch.object(desktop_identity, 'MANIFEST', {}), patch.object(desktop_identity, 'LOADED', {}):
            result = desktop_identity.diagnostics()
            self.assertEqual(result['backend_build_id'], '未知')
            self.assertEqual(result['loaded_matches_build'], '未知')

    def test_runtime_requires_both_net10_frameworks_and_handles_timeout(self):
        with patch('mcp_local.desktop_launcher.subprocess.run') as run:
            for output in ('Microsoft.NETCore.App 10.0.12 [x]', 'Microsoft.WindowsDesktop.App 9.0.7 [x]', ''):
                run.return_value = SimpleNamespace(returncode=0, stdout=output)
                with self.assertRaisesRegex(OSError, 'Desktop Runtime'):
                    desktop_launcher.check_desktop_runtime()
            run.return_value = SimpleNamespace(returncode=0, stdout='Microsoft.NETCore.App 10.0.12 [x]\nMicrosoft.WindowsDesktop.App 10.0.12 [x]')
            desktop_launcher.check_desktop_runtime()
            run.side_effect = desktop_launcher.subprocess.TimeoutExpired('dotnet', 5)
            with self.assertRaisesRegex(OSError, '檢查未完成'):
                desktop_launcher.check_desktop_runtime()


if __name__ == '__main__':
    unittest.main()
