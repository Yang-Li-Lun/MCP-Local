"""ENG-01..16 隔離回歸；不連接正式端點或操作真實設定及排程。"""
import asyncio
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

from connection_settings import (load_for_edit, load_settings, save_settings,
                                 normalize_connection, read_settings_data, SettingsError)
from local_files_mcp import FileReader, create_server
from workspace_reader import WorkspaceReader, TOOLS
from operation_budget import Budget, OperationError, operation, checkpoint, asynchronous, SLOTS
from snapshot_cache import SnapshotCache
from connection_runtime import START_WRAPPER
from gui_tasks import GuiTasks
import autostart_windows as tasks


class EngineeringTests(unittest.TestCase):
    def setUp(self):
        from autostart_windows import BackgroundStatus
        status = patch('local_files_gui.get_background_status',
                       return_value=BackgroundStatus(False, False, False))
        status.start()
        self.addCleanup(status.stop)
        self.temporary = tempfile.TemporaryDirectory(prefix='mcp-engineering-')
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.root = self.base / 'share'
        self.root.mkdir()
        self.target = self.base / 'settings.json'
        self.settings = {'root': str(self.root), 'tunnel': 'tunnel_fixture', 'settings_version': 3,
                         'roots': [{'id': 'main', 'path': str(self.root)}], 'default_root': 'main'}

    def test_repair_missing_root_preserves_backup_and_blocks_start(self):
        save_settings(self.settings, self.target)
        original = self.target.read_bytes()
        self.root.rename(self.base / 'moved')
        editable = load_for_edit(self.target)
        self.assertEqual(editable.state, 'REPAIR_REQUIRED')
        self.assertEqual(editable.errors, [{'root_id': 'main', 'code': 'ROOT_INVALID'}])
        with self.assertRaises(ValueError):
            load_settings(self.target)
        settings = editable.settings
        settings['roots'][0]['path'] = str(self.base / 'moved')
        save_settings(settings, self.target, expected_revision=editable.revision)
        self.assertEqual(next(self.base.glob('*.bak')).read_bytes(), original)
        self.assertEqual(load_for_edit(self.target).state, 'READY')

    def test_unknown_corrupt_and_oversize_remain_untouched(self):
        for data in (b'{', b'{}' * 100000, b'{"settings_version":99}'):
            self.target.write_bytes(data)
            with self.assertRaises(ValueError):
                load_for_edit(self.target)
            with self.assertRaises(ValueError):
                save_settings(self.settings, self.target)
            self.assertEqual(self.target.read_bytes(), data)
            self.assertEqual(list(self.base.glob('*.bak')), [])

    def test_conflict_and_busy_do_not_overwrite(self):
        save_settings(self.settings, self.target)
        editable = load_for_edit(self.target)
        changed = dict(self.settings, recent=['new'])
        save_settings(changed, self.target)
        original = self.target.read_bytes()
        with self.assertRaises(SettingsError) as error:
            save_settings(self.settings, self.target, expected_revision=editable.revision)
        self.assertEqual(error.exception.code, 'SETTINGS_CONFLICT')
        self.assertEqual(self.target.read_bytes(), original)
        lock = self.target.with_name('settings.json.lock')
        lock.touch()
        with self.assertRaises(SettingsError) as error:
            save_settings(self.settings, self.target)
        self.assertEqual(error.exception.code, 'SETTINGS_BUSY')

    def test_failed_backup_and_replace_preserve_original(self):
        save_settings(self.settings, self.target)
        original = self.target.read_bytes()
        changed = dict(self.settings, recent=['changed'])
        real_write = Path.write_text
        def fail_backup(path, *args, **kwargs):
            if path.suffix == '.bak':
                raise OSError('injected')
            return real_write(path, *args, **kwargs)
        for injection in (patch.object(Path, 'write_text', fail_backup),
                          patch.object(Path, 'replace', side_effect=OSError('injected'))):
            with injection, self.assertRaises(OSError):
                save_settings(changed, self.target)
            self.assertEqual(self.target.read_bytes(), original)
            self.assertFalse(self.target.with_name('settings.json.lock').exists())

    def test_state_directory_both_directions_and_direct_entry(self):
        state = self.root / 'state'
        state.mkdir()
        child = state / 'child'
        child.mkdir()
        with patch('security_policy.STATE_DIR', state):
            for root in (self.root, state, child):
                with self.assertRaises(ValueError):
                    FileReader(root)
                with self.assertRaises(ValueError):
                    WorkspaceReader([{'id': 'a', 'path': str(root)}], 'a')

    def test_root_replacement_rejects_old_cursor_and_read(self):
        (self.root / 'a.txt').write_text('old')
        (self.root / 'b.txt').write_text('old')
        reader = FileReader(self.root)
        cursor = reader.list_files(limit=1)['next_cursor']
        self.root.rename(self.base / 'old')
        self.root.mkdir()
        (self.root / 'a.txt').write_text('new')
        with self.assertRaises(ValueError):
            reader.read_file('a.txt')
        with self.assertRaises(ValueError):
            reader.list_files(cursor=cursor)

    def test_open_swap_rejects_replacement(self):
        file = self.root / 'a.txt'
        file.write_text('allowed')
        replacement = self.root / 'next.txt'
        replacement.write_text('replacement')
        reader = FileReader(self.root)
        original_open = Path.open
        def swap(path, *args, **kwargs):
            if path.name == file.name and args == ('rb',):
                file.unlink()
                replacement.rename(file)
            return original_open(path, *args, **kwargs)
        with patch.object(Path, 'open', swap), self.assertRaises(ValueError):
            reader.read_file('a.txt')

    def test_stream_read_splitlines_and_late_invalid(self):
        path = self.root / 'sample.txt'
        reader = FileReader(self.root)
        for text in ('', '\n', 'a\r\nb\rc\v尾\u2028x\x85', '\ufeff前\n後', 'a' * 65535 + '\r\n尾'):
            path.write_text(text, encoding='utf-8', newline='')
            lines = text.removeprefix('\ufeff').splitlines()
            for start in (2, len(lines) + 2):
                result = reader.read_file('sample.txt', start, 2)
                self.assertEqual(result['total_lines'], len(lines))
                expected = '\n'.join(f'{i+1}: {lines[i]}' for i in range(start-1, min(start+1, len(lines))))
                self.assertEqual(result['content'], expected)
        for suffix in (b'\x00', b'\xff'):
            path.write_bytes(b'valid\n' + b'a' * 100000 + suffix)
            with self.assertRaises(ValueError):
                reader.read_file('sample.txt', 1, 1)

    def test_read_window_char_limit_includes_newlines(self):
        (self.root / 'a.txt').write_text(('x' * 55 + '\n') * 400)
        result = FileReader(self.root).read_file('a.txt', 1, 400)
        self.assertLessEqual(len(result['content']), 24000)
        self.assertIsNotNone(result['next_start_line'])

    def test_search_stops_without_opening_next_file_and_shows_long_hit(self):
        (self.root / 'a.txt').write_text('a' * 700 + 'STRASSE' + 'x' * 100)
        (self.root / 'b.txt').write_text('unvisited')
        reader = FileReader(self.root)
        with patch.object(reader, 'open_checked', wraps=reader.open_checked) as opened:
            result = reader.search_text('straße', limit=1)
        self.assertEqual(opened.call_count, 1)
        self.assertTrue(result['truncated'])
        self.assertIn('strasse', result['matches'][0]['match_snippet'])

    def test_strict_inputs_and_unknown_fields(self):
        (self.root / 'a.txt').write_text('text')
        reader = FileReader(self.root)
        for value in (True, None, 1, '', '  ', 'a\nb'):
            with self.assertRaises(ValueError):
                reader.search_text(value)
        for value in (True, '1', None):
            with self.assertRaises(ValueError):
                reader.search_text('x', limit=value)
        for path in (None, 3, 'CON.txt', 'a.txt.', 'a.txt ', 'a.txt:stream'):
            with self.assertRaises(ValueError):
                reader.read_file(path)

    def test_contract_full_schema_and_rejection(self):
        workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        server = create_server(workspace)
        async def check():
            listed = await server.list_tools()
            self.assertEqual({tool.name for tool in listed}, set(TOOLS))
            for tool in listed:
                self.assertTrue(tool.annotations.readOnlyHint)
                self.assertFalse(tool.annotations.destructiveHint)
                self.assertFalse(tool.annotations.openWorldHint)
                self.assertFalse(tool.inputSchema['additionalProperties'])
                if tool.name not in ('workspace_info',):
                    self.assertIn('root_id', tool.inputSchema['properties'])
            search = next(tool for tool in listed if tool.name == 'search_text')
            props = search.inputSchema['properties']
            self.assertEqual(props['context_lines']['default'], 0)
            self.assertEqual(props['context_lines']['maximum'], 3)
            self.assertEqual(props['limit']['default'], 50)
            self.assertEqual(search.inputSchema['required'], ['query'])
            with self.assertRaises(Exception):
                await server.call_tool('search_text', {'query': 'a', 'limit': True})
            with self.assertRaises(Exception):
                await server.call_tool('search_text', {'query': 'a', 'limit': '1'})
            with self.assertRaises(Exception):
                await server.call_tool('workspace_info', {'unknown': 1})
        asyncio.run(check())
        self.assertEqual(workspace.workspace_info()['contract_version'], 4)

    def test_cancel_timeout_busy_and_release(self):
        budget = Budget()
        budget.cancel.set()
        with self.assertRaisesRegex(OperationError, 'CANCELLED'), operation(budget):
            checkpoint()
        with self.assertRaisesRegex(OperationError, 'TIMEOUT'), operation(Budget(timeout=-1)):
            checkpoint()
        acquired = []
        try:
            for _ in range(4):
                self.assertTrue(SLOTS.acquire(False))
                acquired.append(1)
            with self.assertRaisesRegex(OperationError, 'BUSY'), operation():
                pass
        finally:
            for _ in acquired:
                SLOTS.release()
        with operation():
            checkpoint()

    def test_async_cancel_releases_actual_worker(self):
        entered, stopped = threading.Event(), threading.Event()
        def slow():
            entered.set()
            try:
                while True:
                    checkpoint()
                    time.sleep(.005)
            finally:
                stopped.set()
        async def check():
            task = asyncio.create_task(asynchronous(slow)())
            while not entered.is_set():
                await asyncio.sleep(.005)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
            for _ in range(100):
                if stopped.is_set():
                    break
                await asyncio.sleep(.005)
            self.assertTrue(stopped.is_set())
        asyncio.run(check())

    def test_snapshot_bytes_and_mutable_reference_isolation(self):
        rows = [{'path': 'first'}]
        status = {'truncated': False}
        cache = SnapshotCache()
        key = cache.put('a', 'f', rows, status)
        rows[0]['path'] = 'changed'
        entry = cache.get(key, 'a', 'f')
        with self.assertRaises(TypeError):
            entry['rows'][0]['path'] = 'changed'
        entry['status']['truncated'] = True
        self.assertEqual(cache.get(key, 'a', 'f')['rows'][0]['path'], 'first')
        with patch('snapshot_cache.SNAPSHOT_BYTES', 2000):
            key = cache.put('a', 'f', [{'path': 'x' * 10000}], status)
        self.assertTrue(cache.get(key, 'a', 'f')['status']['truncated'])

    def test_start_wrapper_requires_exact_permit(self):
        marker = self.base / 'started'
        command = [sys.executable, '-c', 'from pathlib import Path; import sys; Path(sys.argv[1]).touch()', str(marker)]
        for permit in (b'', b'0', b'x', b'1'):
            result = subprocess.run([sys.executable, '-B', '-c', START_WRAPPER, *command],
                                    input=permit, capture_output=True, timeout=5,
                                    creationflags=subprocess.CREATE_NO_WINDOW)
            self.assertEqual(marker.exists(), permit == b'1')
            self.assertEqual(result.returncode, 0 if permit == b'1' else 125)

    def test_gui_worker_bounded_main_thread_callback_and_close(self):
        worker = GuiTasks()
        gate = threading.Event()
        seen = []
        main_thread = threading.get_ident()
        self.assertTrue(worker.submit(lambda: gate.wait(2), lambda value, error: seen.append(threading.get_ident())))
        self.assertFalse(worker.submit(lambda: None, lambda *args: None))
        gate.set()
        for _ in range(100):
            worker.poll()
            if not worker.busy:
                break
            time.sleep(.005)
        self.assertEqual(seen, [main_thread])
        worker.submit(lambda: 1, lambda *args: seen.append('late'))
        worker.close()
        time.sleep(.03)
        worker.poll()
        self.assertEqual(seen, [main_thread])

    def test_schedule_dedup_mismatch_and_compensation(self):
        with patch.object(tasks, 'is_registered', return_value=True), patch.object(tasks, 'validate_registered_task', return_value=True), patch.object(tasks, '_run') as command:
            tasks.register_autostart()
            command.assert_not_called()
        with patch.object(tasks, 'is_registered', return_value=True), patch.object(tasks, 'validate_registered_task', return_value=False), patch.object(tasks, '_run') as command:
            with self.assertRaisesRegex(ValueError, 'MISMATCH'):
                tasks.register_autostart()
            command.assert_not_called()
        with patch.object(tasks, 'is_registered', side_effect=[False, True]), patch.object(tasks, 'validate_registered_task', return_value=True), patch.object(tasks, 'register_autostart') as register, patch.object(tasks, 'unregister_autostart') as unregister:
            with self.assertRaises(OSError):
                tasks.apply_settings_transaction({'auto_start': True}, {}, Mock(side_effect=OSError('save')))
            register.assert_called_once()
            unregister.assert_called_once()


    def test_gui_repair_slow_save_keeps_ui_responsive_and_stop_cancels_start(self):
        import tkinter as tk
        from local_files_gui import App
        settings = normalize_connection(self.settings)
        window = tk.Tk()
        window.withdraw()
        with patch('tray_windows.Tray'):
            app = App(window, settings, Mock(load=Mock(return_value='fake-key')))
        gate = threading.Event()
        launched = []
        try:
            app.repair_required = True
            with patch.object(app.connection, 'start') as start:
                app.start()
                start.assert_not_called()
            def save(value):
                gate.wait(2)
                save_settings(value, self.target)
            with patch('local_files_gui.save_settings', save):
                self.assertTrue(app.save(after=lambda: launched.append(True)))
                self.assertFalse(app.save())
                painted = []
                window.after(0, lambda: painted.append(True))
                window.update()
                self.assertEqual(painted, [True])
                app.stop()
                gate.set()
                deadline = time.monotonic() + 3
                while app.tasks.busy and time.monotonic() < deadline:
                    app.tasks.poll()
                    time.sleep(.005)
            self.assertFalse(app.tasks.busy)
            self.assertFalse(app.repair_required)
            self.assertEqual(launched, [])
            self.assertEqual(load_for_edit(self.target).state, 'READY')
        finally:
            gate.set()
            app.quit()

    def test_cleanup_failure_still_notifies_done(self):
        from connection_runtime import Connection
        import uuid
        connection = Connection()
        fake_job = Mock()
        fake_job.close.side_effect = OSError('private detail')
        with patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex), patch('connection_runtime.existing_tunnel', return_value=False), patch('connection_runtime.build_commands', return_value=[]), patch('tray_windows.Job', return_value=fake_job):
            connection.start({'settings_version': 3}, 'fake-key')
            connection.thread.join(timeout=5)
        self.assertFalse(connection.thread.is_alive())
        events = list(connection.events.queue)
        self.assertEqual(events[-1][0], 'done')
        self.assertIn('RESOURCE_CLEANUP_FAILED', str(events))
        self.assertNotIn('private detail', str(events))

    def test_job_assignment_failure_never_starts_child(self):
        from connection_runtime import Connection
        import uuid
        marker = self.base / 'never-started'
        command = [sys.executable, '-c', 'from pathlib import Path; import sys; Path(sys.argv[1]).touch()', str(marker)]
        connection = Connection()
        fake_job = Mock()
        fake_job.assign.side_effect = OSError('injected')
        with patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex), patch('connection_runtime.existing_tunnel', return_value=False), patch('connection_runtime.build_commands', return_value=[command]), patch('tray_windows.Job', return_value=fake_job):
            connection.start({'settings_version': 3}, 'fake-key')
            connection.thread.join(timeout=5)
        self.assertFalse(marker.exists())
        self.assertFalse(connection.thread.is_alive())
        self.assertEqual(list(connection.events.queue)[-1][0], 'done')

    def test_rotation_failure_preserves_original(self):
        save_settings(self.settings, self.target)
        original = self.target.read_bytes()
        with patch('connection_settings.rotate_backups', side_effect=OSError('injected')), self.assertRaises(OSError):
            save_settings(dict(self.settings, recent=['change']), self.target)
        self.assertEqual(self.target.read_bytes(), original)

    def test_schedule_compensation_failure_reports_inconsistency(self):
        with patch.object(tasks, 'is_registered', side_effect=[False, True]), patch.object(tasks, 'validate_registered_task', return_value=False), patch.object(tasks, 'register_autostart'):
            with self.assertRaisesRegex(ValueError, 'INCONSISTENT'):
                tasks.apply_settings_transaction({'auto_start': True}, {}, Mock(side_effect=OSError('save')))


    def test_full_casefold_expansion_fits_snippet(self):
        query = 'ﬃ' * 200
        (self.root / 'a.txt').write_text('x' * 700 + query, encoding='utf-8')
        match = FileReader(self.root).search_text(query)['matches'][0]
        self.assertIn(query.casefold(), match['match_snippet'])
        self.assertLessEqual(len(match['match_snippet']), 600)


    def test_cumulative_budget_is_enforced(self):
        budget = Budget()
        budget.max_bytes = 10
        with operation(budget):
            checkpoint(bytes_read=10)
            with self.assertRaisesRegex(OperationError, 'RESOURCE_LIMIT'):
                checkpoint(bytes_read=1)
