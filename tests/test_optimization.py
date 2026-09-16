"""快照、串流、背景連線設定及供應商驗證的回歸測試。"""
import hashlib
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

from local_files_mcp import FileReader
from snapshot_cache import SnapshotCache
from connection_settings import save_settings, load_settings, backup_connection_profile
from connection_runtime import RetryPolicy, ConnectionErrorKind
from workspace_settings import normalize_workspace
from integrity import verify_client, IntegrityError
import autostart_windows as tasks


class OptimizationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def test_snapshot_scans_once_and_revalidates_reads(self):
        for i in range(1001):
            (self.root / f'{i:04}.txt').touch()
        reader = FileReader(self.root)
        with patch.object(reader, 'walk', wraps=reader.walk) as walk:
            page = reader.list_files(limit=200)
            anchor = page['files'][0]
            (self.root / anchor).unlink()
            seen = page['files'][:]
            while page['next_cursor']:
                page = reader.list_files(limit=200, cursor=page['next_cursor'])
                seen.extend(page['files'])
            self.assertEqual(len(seen), 1001)
            self.assertEqual(walk.call_count, 1)
        with self.assertRaises(OSError):
            reader.read_file(anchor)

    def test_snapshot_expiration_lru_and_capacity(self):
        now = [0]
        cache = SnapshotCache(lambda: now[0])
        first = cache.put('a', 'f', [], {})
        for _ in range(8):
            cache.put('a', 'f', [], {})
        with self.assertRaises(ValueError):
            cache.get(first, 'a', 'f')
        key = cache.put('b', 'f', [], {})
        with self.assertRaises(ValueError):
            cache.get(key, 'a', 'f')
        now[0] = 61
        with self.assertRaisesRegex(ValueError, '失效'):
            cache.get(key, 'b', 'f')
        key = cache.put('a', 'f', [], {})
        for step in range(1, 6):
            now[0] = 61 + step * 59
            cache.get(key, 'a', 'f')
        now[0] = 361
        with self.assertRaises(ValueError):
            cache.get(key, 'a', 'f')
        for i in range(40):
            cache.put(str(i), 'f', [None] * 10000, {})
        self.assertLessEqual(len(cache.items), 32)
        self.assertLessEqual(sum(len(e['rows']) for e in cache.items.values()), 250000)

    def test_snapshot_settings_change_invalidates_cursor(self):
        for name in ('a.txt', 'b.txt'):
            (self.root / name).touch()
        reader = FileReader(self.root)
        cursor = reader.list_files(limit=1)['next_cursor']
        reader.settings['max_scan_files'] = 10
        with self.assertRaises(ValueError):
            reader.list_files(cursor=cursor)

    def test_stream_matches_splitlines_and_context(self):
        text = '\ufeff前文\r\nStraße\n中文\vafter\fMARK\x85中文\u2028end\r尾'
        (self.root / 'sample.txt').write_text(text, encoding='utf-8', newline='')
        for query in ('STRASSE', '中文', 'mark', '尾'):
            for context in range(4):
                result = FileReader(self.root).search_text(query, context_lines=context)
                lines = text.removeprefix('\ufeff').splitlines()
                expected = [(i + 1, line) for i, line in enumerate(lines) if query.casefold() in line.casefold()]
                self.assertEqual([(m['line'], m['text']) for m in result['matches']], expected)
                if context:
                    for match in result['matches']:
                        number = match['line']
                        self.assertEqual([c['text'] for c in match['context']],
                                         lines[max(0, number - 1 - context):number + context])

    def test_stream_chunk_boundaries_long_lines_and_late_invalid(self):
        file = self.root / 'sample.txt'
        data = b'a' * 65535 + '中文MARK'.encode() + b'x' * 131072 + b'\r\nlast'
        file.write_bytes(data)
        reader = FileReader(self.root)
        matches = reader.search_text('中文mark', context_lines=1)['matches']
        self.assertEqual(len(matches), 1)
        self.assertTrue(matches[0]['line_truncated'])
        self.assertEqual(matches[0]['context'][-1]['text'], 'last')
        for suffix in (b'\xff', b'\x00'):
            file.write_bytes(data + suffix)
            self.assertEqual(reader.search_text('mark')['matches'], [])

    def test_overlap_rejected(self):
        child = self.root / 'child'
        child.mkdir()
        root = {'id': 'a', 'path': str(self.root)}
        for other in (self.root, child, Path(str(self.root).upper())):
            if os.name != 'nt' and other != self.root and other != child:
                continue
            with self.assertRaises(ValueError):
                normalize_workspace([root, {'id': 'b', 'path': str(other)}], 'a')

    def test_backups_rotate_deduplicate_and_profile_cap(self):
        target = self.root / 'settings.json'
        settings = {'root': str(self.root), 'tunnel': 'tunnel_test', 'settings_version': 1}
        for i in range(20):
            settings['recent'] = [str(i)]
            save_settings(settings, target)
        self.assertEqual(len(list(self.root.glob('settings.json.*.bak'))), 5)
        before = target.stat().st_mtime_ns
        save_settings(load_settings(target), target)
        self.assertEqual(target.stat().st_mtime_ns, before)
        profile = self.root / 'local-files-gui.yaml'
        profile.write_text('fixture')
        for _ in range(10):
            backup_connection_profile([['client', '--profile-dir', str(self.root), '--profile', 'local-files-gui']])
        self.assertEqual(len(list(self.root.glob('local-files-gui.yaml.*.bak'))), 3)

    def test_integrity_valid_invalid_missing_manifest(self):
        client = self.root / 'tunnel-client.exe'
        client.write_bytes(b'fixture')
        manifest = self.root / 'vendor-integrity.json'
        manifest.write_text(json.dumps({'tunnel-client.exe': {'sha256': hashlib.sha256(b'fixture').hexdigest()}}))
        verify_client(self.root)
        client.write_bytes(b'changed')
        with self.assertRaises(IntegrityError):
            verify_client(self.root)
        manifest.unlink()
        with self.assertRaises(IntegrityError):
            verify_client(self.root)

    def test_retry_backoff_bounded_and_permanent_errors(self):
        policy = RetryPolicy()
        with patch('connection_runtime.random.uniform', return_value=1):
            self.assertEqual([policy.next_delay() for _ in range(7)], [5, 15, 30, 60, 300, 300, 300])
            policy.reset()
            self.assertEqual(policy.next_delay(), 5)
        for kind in (ConnectionErrorKind.KEY_DECRYPT_FAILED, ConnectionErrorKind.AUTH_FAILED,
                     ConnectionErrorKind.CLIENT_INTEGRITY_FAILED, ConnectionErrorKind.CONFIG_INVALID):
            self.assertNotIn(kind, policy.retryable)

    def test_task_xml_security_and_malformed_validation(self):
        with patch.object(tasks, 'current_sid', return_value='S-1-5-21-123'):
            xml = tasks.task_xml()
            self.assertNotIn('CONTROL_PLANE_API_KEY', xml)
            tree = ET.fromstring(xml)
            with patch.object(tasks, '_query', return_value=tree):
                self.assertTrue(tasks.validate_registered_task())
                tree.find(f'{{{tasks.NS}}}Principals/{{{tasks.NS}}}Principal/{{{tasks.NS}}}RunLevel').text = 'HighestAvailable'
                self.assertFalse(tasks.validate_registered_task())

    def test_background_import_does_not_load_tk(self):
        import subprocess
        import sys
        result = subprocess.run([sys.executable, '-B', '-c',
                                 "import autostart,sys; assert 'tkinter' not in sys.modules"], capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_autostart_uses_tray_entry(self):
        import autostart
        with patch('local_files_gui.main', return_value=0) as run:
            self.assertEqual(autostart.main(), 0)
            run.assert_called_once_with(startup=True)

    def test_startup_duplicate_is_silent(self):
        import local_files_gui as gui
        with patch.object(gui.tk, 'Tk') as window, patch('tray_windows.kernel') as kernel, patch.object(
                gui.ctypes, 'get_last_error', return_value=183), patch.object(gui.messagebox, 'showinfo') as info:
            self.assertEqual(gui.main(startup=True), 0)
            info.assert_not_called()
            window.return_value.destroy.assert_called_once()
            kernel.CloseHandle.assert_called_once()

    def test_disabled_startup_does_not_create_app(self):
        import local_files_gui as gui
        from unittest.mock import Mock
        with patch.object(gui.tk, 'Tk') as window, patch('tray_windows.kernel'), patch.object(
                gui.ctypes, 'get_last_error', return_value=0), patch.object(gui, 'load_for_edit',
                return_value=Mock(settings={'auto_start': False})), patch.object(gui, 'App') as app:
            self.assertEqual(gui.main(startup=True), 0)
            app.assert_not_called()
            window.return_value.destroy.assert_called_once()

    def test_backup_link_is_preserved(self):
        from backup_rotation import rotate_backups
        source = self.root / 'source.txt'
        source.write_text('preserve')
        link = self.root / ('settings.json.' + 'a' * 32 + '.bak')
        os.link(source, link)
        with self.assertRaises(ValueError):
            rotate_backups(self.root / 'settings.json', 0, self.root)
        self.assertEqual(link.read_text(), 'preserve')

    def test_remote_approval_only_allows_exact_fixture(self):
        from secure_tunnel_smoke import FIXTURE, validate_approval
        item = {'type': 'mcp_approval_request', 'name': 'read_file', 'server_label': 'local_fixture',
                'arguments': json.dumps(FIXTURE)}
        self.assertTrue(validate_approval(item))
        for args in (dict(FIXTURE, path='private.txt'), dict(FIXTURE, root_id='other'),
                     dict(FIXTURE, line_count=400)):
            self.assertFalse(validate_approval(dict(item, arguments=json.dumps(args))))
        self.assertFalse(validate_approval(dict(item, name='read_files')))
