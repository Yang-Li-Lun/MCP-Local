"""GUI 操作回歸；排程、金鑰、通道一律使用替身。"""
import tempfile
import threading
import time
import tkinter as tk
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

import autostart_windows as background
from connection_settings import normalize_connection, save_settings, load_for_edit, SettingsError
from local_files_gui import App


class GuiControlTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.target = Path(self.temp.name) / 'settings.json'
        self.settings = normalize_connection({'root': self.temp.name, 'tunnel': 'tunnel_fixture', 'settings_version': 3, 'roots': [{'id': 'main', 'path': self.temp.name}], 'default_root': 'main'})
        save_settings(self.settings, self.target)
        for target, value in [('tray_windows.Tray', Mock()),
                              ('local_files_gui.get_background_status', background.BackgroundStatus(False, False, False))]:
            mocked = patch(target, return_value=value)
            mocked.start()
            self.addCleanup(mocked.stop)
        self.window = tk.Tk()
        self.window.withdraw()
        self.app = App(self.window, self.settings, Mock(load=Mock(return_value='fake-key')))
        self.app.settings_revision = load_for_edit(self.target).revision
        self.window.update()
        self.addCleanup(self.close)

    def close(self):
        self.app.running = False
        self.app.finish()

    def poll(self):
        deadline = time.monotonic() + 4
        while self.app.tasks.busy and time.monotonic() < deadline:
            self.app.tasks.poll()
            time.sleep(.005)
        self.assertFalse(self.app.tasks.busy)

    def test_state_matrix(self):
        a = self.app
        for state, running, expected in [
                ('IDLE', False, (True, False, False)),
                ('SAVING', False, (False, False, False)),
                ('STARTING', True, (False, False, True)),
                ('RUNNING', True, (False, True, True)),
                ('STOPPING', True, (False, False, False)),
                ('RESTARTING', True, (False, False, False)),
                ('REPAIR', False, (False, False, False)),
                ('QUITTING', True, (False, False, False))]:
            with self.subTest(state=state):
                a.state, a.running = state, running
                a.repair_required = state == 'REPAIR'
                a.quitting = state == 'QUITTING'
                a.refresh_controls()
                for name, enabled in zip(('start', 'apply', 'stop'), expected):
                    self.assertEqual(str(getattr(a, name + '_button')['state']), 'normal' if enabled else 'disabled')
        a.quitting = False

    def test_only_three_modes_and_no_manual_toolchain_controls(self):
        from access_mode import MODE_LABELS
        a = self.app
        permissions = a.tabs.nametowidget(a.tabs.tabs()[3])
        options = [widget for widget in permissions.winfo_children() if widget.winfo_class() == 'TRadiobutton']
        self.assertEqual([widget.cget('value') for widget in options], list(MODE_LABELS))
        self.assertEqual([widget.cget('text') for widget in options], list(MODE_LABELS.values()))
        for name in ('toolchain_list', 'toolchain_buttons', 'developer_toolchains', 'add_toolchain', 'remove_toolchain'):
            self.assertFalse(hasattr(a, name))

    def test_removed_mode_cannot_be_saved_from_gui(self):
        self.app.access_mode.set('full_control')
        with patch('local_files_gui.messagebox.showerror'), patch('local_files_gui.save_settings') as save:
            self.assertFalse(self.app.save())
        save.assert_not_called()

    def test_access_mode_opt_in_persists_and_reload_uses_saved_mode(self):
        a = self.app
        a.access_mode.set('developer_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=True), patch(
                'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
            self.assertTrue(a.save())
            self.poll()
        self.assertEqual(load_for_edit(self.target).settings['access_mode'], 'developer_control')
        a.access_mode.set('read_only')
        with patch('local_files_gui.load_for_edit', lambda: load_for_edit(self.target)):
            a.reload_settings()
        self.assertEqual(a.access_mode.get(), 'developer_control')
        self.assertFalse(a.dirty)
        a.finish()
        self.window = tk.Tk()
        self.window.withdraw()
        self.app = App(self.window, load_for_edit(self.target).settings, Mock(load=Mock(return_value='fake-key')))
        self.assertEqual(self.app.access_mode.get(), 'developer_control')

    def test_access_mode_opt_in_can_be_cancelled(self):
        a = self.app
        a.access_mode.set('developer_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=False), patch('local_files_gui.save_settings') as save:
            self.assertFalse(a.save())
        save.assert_not_called()
        self.assertEqual(load_for_edit(self.target).settings['access_mode'], 'read_only')

    def test_new_modes_require_local_confirmation_and_persist_after_poll(self):
        for mode in ('host_control', 'developer_control'):
            with self.subTest(mode=mode):
                self.app.access_mode.set(mode)
                with patch('local_files_gui.messagebox.askyesno', return_value=True) as confirmation, patch(
                        'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
                    self.assertTrue(self.app.save())
                    self.poll()
                confirmation.assert_called_once()
                self.assertEqual(load_for_edit(self.target).settings['access_mode'], mode)
                self.assertEqual(self.app.settings_revision, load_for_edit(self.target).revision)
                if mode == 'host_control':
                    self.assertIn('命令不是沙箱', confirmation.call_args.args[1])
                else:
                    self.assertIn('自動偵測', confirmation.call_args.args[1])

    def test_host_confirmation_cancellation_preserves_saved_mode(self):
        self.app.access_mode.set('host_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=False), patch('local_files_gui.save_settings') as save:
            self.assertFalse(self.app.save())
        save.assert_not_called()
        self.assertEqual(load_for_edit(self.target).settings['access_mode'], 'read_only')

    def test_host_to_readonly_stops_before_relaunch(self):
        self.app.settings['access_mode'] = 'host_control'
        self.app.running, self.app.state = True, 'RUNNING'
        self.app.access_mode.set('read_only')
        with patch('local_files_gui.messagebox.askyesno') as confirmation, patch(
                'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
            self.assertTrue(self.app.save())
            self.poll()
        confirmation.assert_not_called()
        self.assertTrue(self.app.connection.cancel.is_set())
        self.assertTrue(self.app.restart_pending)

    def test_mode_save_stops_old_connection_only_after_poll_and_restarts(self):
        a = self.app
        a.running, a.state = True, 'RUNNING'
        a.access_mode.set('developer_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=True), patch(
                'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
            self.assertTrue(a.save())
            self.assertFalse(a.connection.cancel.is_set())
            self.poll()
        self.assertTrue(a.connection.cancel.is_set())
        self.assertTrue(a.restart_pending)
        self.assertEqual(a.state, 'RESTARTING')
        a.connection.events.put(('done', 'fixture stopped'))
        with patch.object(a, 'reload_and_launch') as launch:
            a.tick()
            launch.assert_called_once()
        self.assertFalse(a.restart_pending)

    def test_return_to_readonly_restarts_full_connection(self):
        a = self.app
        a.settings['access_mode'] = 'developer_control'
        a.running, a.state = True, 'RUNNING'
        a.access_mode.set('read_only')
        with patch('local_files_gui.messagebox.askyesno') as confirmation, patch(
                'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
            self.assertTrue(a.save())
            self.poll()
        confirmation.assert_not_called()
        self.assertTrue(a.connection.cancel.is_set())
        self.assertTrue(a.restart_pending)
        self.assertEqual(load_for_edit(self.target).settings['access_mode'], 'read_only')

    def test_failed_cleanup_blocks_mode_downgrade_relaunch(self):
        from connection_runtime import ConnectionErrorKind
        a = self.app
        a.running, a.state = True, 'RESTARTING'
        a.active_access_mode = 'host_control'
        a.restart_pending = a.retry_enabled = True
        a.retry_timer = self.window.after(10000, a.retry_connection)
        a.connection.cleanup_failed = True
        a.connection.error_kind = ConnectionErrorKind.RESOURCE_CLEANUP_FAILED
        a.connection.events.put(('error', 'RESOURCE_CLEANUP_FAILED'))
        a.connection.events.put(('done', '清理失敗，請檢查本次程序'))
        with patch('local_files_gui.messagebox.showerror'), patch.object(a, 'reload_and_launch') as launch:
            a.tick()
            launch.assert_not_called()
        self.assertFalse(a.restart_pending)
        self.assertFalse(a.retry_enabled)
        self.assertIsNone(a.retry_timer)
        self.assertFalse(a.control_states()['start'])
        self.assertIn('停止未確認', a.access_mode_status.get())
        self.assertEqual(a.active_access_mode, 'host_control')
        with patch('local_files_gui.Connection') as replacement:
            a.launch()
            replacement.assert_not_called()

    def test_failed_mode_save_does_not_change_active_mode(self):
        a = self.app
        a.running, a.state = True, 'RUNNING'
        a.access_mode.set('developer_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=True), patch(
                'local_files_gui.save_settings', side_effect=OSError('fixture')), patch('local_files_gui.messagebox.showerror'):
            self.assertTrue(a.save())
            self.poll()
        self.assertFalse(a.connection.cancel.is_set())
        self.assertFalse(a.restart_pending)
        self.assertEqual(a.settings['access_mode'], 'read_only')

    def test_unknown_background_mode_switch_refused_before_saving(self):
        a = self.app
        a.access_mode.set('developer_control')
        with patch('local_files_gui.messagebox.askyesno', return_value=True), patch(
                'local_files_gui.get_background_status', return_value=background.BackgroundStatus(False, False, False, 'UNKNOWN')), patch(
                'local_files_gui.save_settings') as save, patch('local_files_gui.messagebox.showerror'):
            self.assertTrue(a.save())
            self.poll()
        save.assert_not_called()
        self.assertEqual(a.settings['access_mode'], 'read_only')

    def test_icon_selection_applies_after_save_and_survives_reload(self):
        a = self.app
        a.icon_style.set('folder_link')
        self.assertTrue(a.dirty)
        a.tray.set_icon.assert_not_called()
        with patch('local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)):
            self.assertTrue(a.save())
            self.poll()
        self.assertEqual(load_for_edit(self.target).settings['icon_style'], 'folder_link')
        a.tray.set_icon.assert_called_with('folder_link')
        a.icon_style.set('classic')
        with patch('local_files_gui.load_for_edit', lambda: load_for_edit(self.target)):
            a.reload_settings()
        self.assertEqual(a.icon_style.get(), 'folder_link')
        self.assertFalse(a.dirty)

    def test_failed_icon_save_does_not_apply(self):
        a = self.app
        a.icon_style.set('folder_link')
        with patch('local_files_gui.save_settings', side_effect=OSError('fixture')), patch(
                'local_files_gui.messagebox.showerror'):
            self.assertTrue(a.save())
            self.poll()
        a.tray.set_icon.assert_not_called()
        self.assertEqual(load_for_edit(self.target).settings['icon_style'], 'classic')

    def test_refresh_preserves_hover_and_press_until_release(self):
        a = self.app
        button = a.start_button
        button.state(['active', 'pressed'])
        with patch.object(button, 'configure', wraps=button.configure) as configure:
            for _ in range(10):
                a.refresh_controls()
            configure.assert_not_called()
        self.assertTrue(button.instate(['active', 'pressed']))
        with patch.object(a, 'reload_and_launch') as launch:
            button.invoke()
            launch.assert_called_once()
        a.state = 'STARTING'
        a.running = True
        a.refresh_controls()
        self.assertTrue(button.instate(['disabled']))

    def test_existing_connection_uses_shared_stop_button(self):
        a = self.app
        a.background_status = background.BackgroundStatus(True, True, True)
        a.refresh_controls()
        self.assertFalse(a.control_states()['start'])
        self.assertTrue(a.control_states()['stop'])
        with patch('local_files_gui.stop_background') as stop:
            a.stop_button.invoke()
            self.poll()
            stop.assert_called_once()
        for name in ('background_start_button', 'background_stop_button', 'hide_button'):
            self.assertFalse(hasattr(a, name))

    def test_status_query_error_is_visible_and_retryable(self):
        with patch('local_files_gui.get_background_status', side_effect=OSError('fixture')):
            self.app.check_background()
            self.poll()
        self.assertIn('無法查詢', self.app.auto_start_status.get())
        self.assertTrue(self.app.control_states()['background_check'])

    def test_startup_is_hidden_and_connects_once(self):
        self.app.finish()
        self.window = tk.Tk()
        settings = dict(self.settings, auto_start=True, auto_connect=True, start_hidden=False)
        with patch.object(App, 'start') as start:
            self.app = App(self.window, settings, Mock(load=Mock(return_value='fake-key')), startup=True)
            self.window.update()
            self.assertEqual(self.window.state(), 'withdrawn')
            start.assert_called_once()
        self.assertFalse(self.app.hidden.get())

    def test_startup_retry_is_cancelled_by_stop(self):
        from connection_runtime import ConnectionErrorKind
        a = self.app
        a.retry_enabled = True
        a.connection.error_kind = ConnectionErrorKind.DOCTOR_FAILED
        a.connection.events.put(('done', 'fixture'))
        self.window.after_cancel(a.tick_timer)
        a.tick()
        self.assertIsNotNone(a.retry_timer)
        self.assertTrue(a.control_states()['stop'])
        a.stop()
        self.assertIsNone(a.retry_timer)
        self.assertFalse(a.retry_enabled)
        with patch.object(a, 'start') as start:
            a.retry_connection()
            start.assert_not_called()

    def test_retry_preserves_unsaved_draft(self):
        a = self.app
        a.retry_enabled = True
        a.tunnel.set('tunnel_draft')
        with patch.object(a, 'save') as save, patch.object(a, 'reload_and_launch') as launch:
            a.retry_connection()
            save.assert_not_called()
            launch.assert_not_called()
        self.assertEqual(a.tunnel.get(), 'tunnel_draft')
        self.assertFalse(a.retry_enabled)

    def test_retry_waits_for_settings_task(self):
        a = self.app
        a.retry_enabled = True
        a.tasks.busy = True
        with patch.object(a, 'reload_and_launch') as launch:
            a.retry_connection()
            launch.assert_not_called()
        self.assertIsNotNone(a.retry_timer)
        a.tasks.busy = False
        self.window.after_cancel(a.retry_timer)
        with patch.object(a, 'reload_and_launch') as launch:
            a.retry_connection()
            launch.assert_called_once()

    def test_permanent_failure_does_not_retry(self):
        from connection_runtime import ConnectionErrorKind
        a = self.app
        a.retry_enabled = True
        a.connection.error_kind = ConnectionErrorKind.AUTH_FAILED
        a.connection.events.put(('done', 'fixture'))
        self.window.after_cancel(a.tick_timer)
        a.tick()
        self.assertIsNone(a.retry_timer)

    def test_idle_restart_is_noop(self):
        with patch.object(self.app, 'save') as save, patch.object(self.app, 'launch') as launch:
            self.app.restart()
            save.assert_not_called()
            launch.assert_not_called()

    def test_dirty_and_key_enable_save(self):
        a = self.app
        a.tunnel.set('tunnel_changed')
        self.assertTrue(a.dirty)
        self.assertTrue(a.control_states()['save'])
        a.dirty = False
        a.key.set('new-fake-key')
        self.assertTrue(a.key_dirty)
        self.assertTrue(a.control_states()['save'])

    def test_text_edit_marks_dirty(self):
        a = self.app
        a.reader_lists['extensions'].insert('end', ', .fixture')
        self.window.update()
        self.assertTrue(a.dirty)

    def test_save_clears_dirty_only_after_completion(self):
        a = self.app
        a.tunnel.set('tunnel_changed')
        gate = threading.Event()
        self.addCleanup(gate.set)
        def save(value, **kwargs):
            gate.wait(2)
            save_settings(value, self.target, **kwargs)
        with patch('local_files_gui.save_settings', save):
            self.assertTrue(a.save())
            self.assertTrue(a.dirty)
            self.assertFalse(any(value for key, value in a.control_states().items() if key != 'hide'))
            gate.set()
            self.poll()
        self.assertFalse(a.dirty)
        self.assertEqual(load_for_edit(self.target).settings['tunnel'], 'tunnel_changed')

    def test_edit_during_save_preserves_draft_and_cancels_launch(self):
        a = self.app
        a.tunnel.set('tunnel_first')
        gate = threading.Event()
        self.addCleanup(gate.set)
        launched = Mock()
        def save(value, **kwargs):
            gate.wait(2)
            save_settings(value, self.target, **kwargs)
        with patch('local_files_gui.save_settings', save):
            a.save(after=launched)
            a.tunnel.set('tunnel_second')
            gate.set()
            self.poll()
        self.assertTrue(a.dirty)
        self.assertEqual(a.tunnel.get(), 'tunnel_second')
        launched.assert_not_called()

    def test_conflict_without_dirty_reloads_and_retries_once(self):
        a = self.app
        save_settings(dict(self.settings, tunnel='tunnel_external'), self.target)
        launched = Mock()
        with patch('local_files_gui.load_for_edit', lambda: load_for_edit(self.target)), patch(
                'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)) as save:
            a.save(after=launched)
            self.poll()
        self.assertEqual(save.call_count, 2)
        self.assertEqual(a.tunnel.get(), 'tunnel_external')
        launched.assert_called_once()

    def test_repeated_conflict_is_bounded(self):
        a = self.app
        with patch('local_files_gui.load_for_edit', lambda: load_for_edit(self.target)), patch(
                'local_files_gui.save_settings', side_effect=SettingsError('SETTINGS_CONFLICT', 'conflict')) as save, patch(
                'local_files_gui.messagebox.showerror'):
            a.save()
            self.poll()
        self.assertEqual(save.call_count, 2)

    def test_dirty_conflict_preserves_external_and_key(self):
        for reload in (False, True):
            with self.subTest(reload=reload):
                a = self.app
                a.settings_revision = load_for_edit(self.target).revision
                a.tunnel.set('tunnel_local')
                a.key.set('fake-key-draft')
                save_settings(dict(self.settings, tunnel='tunnel_external' + str(reload)), self.target)
                original = self.target.read_bytes()
                launched = Mock()
                with patch('local_files_gui.load_for_edit', lambda: load_for_edit(self.target)), patch(
                        'local_files_gui.save_settings', side_effect=lambda value, **kw: save_settings(value, self.target, **kw)), patch(
                        'local_files_gui.messagebox.askyesno', return_value=reload):
                    a.save(after=launched)
                    self.poll()
                launched.assert_not_called()
                self.assertEqual(self.target.read_bytes(), original)
                self.assertEqual(a.key.get(), 'fake-key-draft')
                self.assertEqual(a.tunnel.get(), 'tunnel_external' + str(reload) if reload else 'tunnel_local')

    def test_background_status_labels(self):
        a = self.app
        for registered, valid, running, enabled, text in [
                (False, False, False, False, '未啟用'),
                (True, True, False, True, '已啟用'),
                (True, True, True, True, '已啟用'),
                (True, False, False, True, '工作設定不一致'),
                (False, False, False, True, '工作遺失')]:
            a.settings['auto_start'] = enabled
            a.background_status = background.BackgroundStatus(registered, valid, running)
            a.refresh_controls()
            self.assertEqual(a.auto_start_status.get(), '登入自啟動：' + text)


class BackgroundControlTests(unittest.TestCase):
    def test_start_stop_noops(self):
        with patch.object(background, 'validate_registered_task', return_value=True), patch.object(
                background, 'background_running', return_value=True), patch.object(background, '_run') as command:
            background.start_background()
            command.assert_not_called()
        with patch.object(background, 'validate_registered_task', return_value=True), patch.object(
                background, 'background_running', return_value=False), patch.object(background, '_run') as command:
            background.stop_background()
            command.assert_not_called()

    def test_mismatch_rejects_all_actions(self):
        with patch.object(background, 'validate_registered_task', return_value=False), patch.object(background, '_run') as command:
            for action in (background.start_background, background.stop_background, background.restart_background):
                with self.assertRaisesRegex(ValueError, 'TASK_MISMATCH'):
                    action()
            command.assert_not_called()

    def test_restart_order(self):
        calls = []
        with patch.object(background, 'validate_registered_task', return_value=True), patch.object(
                background, 'stop_background', side_effect=lambda: calls.append('stop')), patch.object(
                background, 'start_background', side_effect=lambda: calls.append('start')):
            background.restart_background()
        self.assertEqual(calls, ['stop', 'start'])

    def test_disable_order_and_compensation(self):
        for fail in (False, True):
            calls = []
            registered = [True]
            def remove():
                calls.append('unregister')
                registered[0] = False
            def register():
                calls.append('register')
                registered[0] = True
            def save(value):
                calls.append('save')
                if fail:
                    raise OSError('injected')
            with patch.object(background, 'is_registered', side_effect=lambda: registered[0]), patch.object(
                    background, 'validate_registered_task', return_value=True), patch.object(
                    background, 'background_running', return_value=True), patch.object(
                    background, 'stop_background', side_effect=lambda: calls.append('stop')), patch.object(
                    background, 'unregister_autostart', side_effect=remove), patch.object(
                    background, 'register_autostart', side_effect=register), patch.object(
                    background, 'start_background', side_effect=lambda: calls.append('start')):
                if fail:
                    with self.assertRaises(OSError):
                        background.apply_settings_transaction({'auto_start': False}, {'auto_start': True}, save)
                else:
                    background.apply_settings_transaction({'auto_start': False}, {'auto_start': True}, save)
            self.assertEqual(calls, ['stop', 'unregister', 'save'] + (['register', 'start'] if fail else []))

    def test_wait_timeout_and_status_failure(self):
        with patch.object(background, 'background_running', return_value=False), patch.object(
                background.time, 'monotonic', side_effect=[0, 11]):
            with self.assertRaisesRegex(ValueError, 'BACKGROUND_START_FAILED'):
                background._wait_background(True)
        with patch.object(background, 'is_registered', side_effect=OSError('private')):
            self.assertEqual(background.get_background_status().error_code, 'BACKGROUND_STATUS_FAILED')

    def test_stop_failure_preserves_task_and_settings(self):
        with patch.object(background, 'is_registered', return_value=True), patch.object(
                background, 'validate_registered_task', return_value=True), patch.object(
                background, 'background_running', return_value=True), patch.object(
                background, 'stop_background', side_effect=ValueError('BACKGROUND_STOP_FAILED')), patch.object(
                background, 'start_background') as start, patch.object(
                background, 'unregister_autostart') as remove:
            save = Mock()
            with self.assertRaisesRegex(ValueError, 'BACKGROUND_STOP_FAILED'):
                background.apply_settings_transaction({'auto_start': False}, {'auto_start': True}, save)
            remove.assert_not_called()
            save.assert_not_called()
            start.assert_called_once()

    def test_background_compensation_failure_is_explicit(self):
        with patch.object(background, 'is_registered', side_effect=[True, False]), patch.object(
                background, 'validate_registered_task', return_value=True), patch.object(
                background, 'background_running', return_value=True), patch.object(
                background, 'stop_background'), patch.object(background, 'unregister_autostart'), patch.object(
                background, 'register_autostart', side_effect=OSError('private')):
            with self.assertRaisesRegex(ValueError, 'SETTINGS_TASK_INCONSISTENT'):
                background.apply_settings_transaction({'auto_start': False}, {'auto_start': True}, Mock(side_effect=OSError()))

    def test_explicit_commands_and_wait(self):
        for action, states, verb in [(background.start_background, [False, True], '/Run'),
                                     (background.stop_background, [True, False], '/End')]:
            with patch.object(background, 'validate_registered_task', return_value=True), patch.object(
                    background, 'background_running', side_effect=states), patch.object(
                    background, 'task_name', return_value='fixture-task'), patch.object(
                    background, '_run', return_value=Mock(returncode=0)) as command:
                action()
                command.assert_called_once_with(['schtasks.exe', verb, '/TN', 'fixture-task'])
