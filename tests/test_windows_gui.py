"""Windows 本機煙霧測試，不啟動真實通道或使用金鑰。"""
import ctypes
from ctypes import wintypes
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import tkinter as tk
import unittest
from unittest.mock import patch, Mock

from local_files_gui import App, Connection, load_settings
from tray_windows import Job, kernel, signature


class WindowsGuiTests(unittest.TestCase):
    def setUp(self):
        import uuid
        mutex = patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex)
        mutex.start()
        self.addCleanup(mutex.stop)

    def test_real_tray_notifications_open_menu_then_settings(self):
        # 獨立程序執行真正的 Tcl/Win32 訊息迴圈，能捕捉原生 GIL 崩潰。
        program = r'''
import ctypes, os, threading, time, tkinter as tk
from ctypes import wintypes
from pathlib import Path
from local_files_gui import App, load_settings
from tray_windows import user, signature
signature(user, 'FindWindowW', wintypes.HWND, wintypes.LPCWSTR, wintypes.LPCWSTR)
signature(user, 'GetWindowThreadProcessId', wintypes.DWORD, wintypes.HWND,
          ctypes.POINTER(wintypes.DWORD))
w = tk.Tk()
a = App(w, load_settings(Path('nonexistent-test-settings.json')))
w.withdraw()
a.key.set('fake-key-retained')
observed = []
def notifications():
    time.sleep(.2)
    user.PostMessageW(a.tray.window, 0x8001, 1, 0x205)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        menu = user.FindWindowW('#32768', None)
        owner = wintypes.DWORD()
        if menu:
            user.GetWindowThreadProcessId(menu, ctypes.byref(owner))
            if owner.value == os.getpid():
                observed.append('native-menu-visible')
                break
        time.sleep(.05)
    user.PostMessageW(a.tray.window, 0x1f, 0, 0)
def check_menu():
    if worker.is_alive():
        w.after(50, check_menu)
        return
    observed.append(w.state())
    observed.append(a.key.get() == 'fake-key-retained')
    user.PostMessageW(a.tray.window, 0x8001, 1, 0x202)
    w.after(300, finish)
def finish():
    observed.append(w.state())
    a.quit()
worker = threading.Thread(target=notifications, daemon=True)
worker.start()
w.after(100, check_menu)
w.mainloop()
assert observed == ['native-menu-visible', 'withdrawn', True, 'normal'], observed
print('REAL_TRAY_EVENTS_OK')
'''
        result = subprocess.run([sys._base_executable, '-B', '-c', program],
                                capture_output=True, text=True, timeout=10,
                                creationflags=subprocess.CREATE_NO_WINDOW)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('REAL_TRAY_EVENTS_OK', result.stdout)

    def test_connection_sequence_and_failure(self):
        python = str(Path('.venv/Scripts/python.exe').resolve())
        connection = Connection()
        commands = [[python, '-c', 'import os,ctypes; assert os.environ["CONTROL_PLANE_API_KEY"] == "fake-key"; assert not ctypes.windll.kernel32.GetConsoleWindow()'],
                    [python, '-c', 'raise SystemExit(7)'],
                    [python, '-c', 'raise RuntimeError("must not run")']]
        with patch('connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=commands):
            connection.start({'settings_version': 1}, 'fake-key')
            connection.thread.join(timeout=10)
        self.assertFalse(connection.thread.is_alive())
        events = list(connection.events.queue)
        self.assertEqual([text for kind, text in events if kind == 'status'], ['建立設定', '檢查連線'])
        self.assertTrue(any(kind == 'error' and '7' in text for kind, text in events))
        self.assertIsNone(connection.job)

    def test_connection_stop_and_duplicate_guard(self):
        python = str(Path('.venv/Scripts/python.exe').resolve())
        connection = Connection()
        with patch('connection_runtime.existing_tunnel', return_value=True):
            connection.start({'settings_version': 1}, 'fake-key')
            connection.thread.join(timeout=5)
        self.assertTrue(any(kind == 'error' and 'Ctrl+C' in text for kind, text in connection.events.queue))
        connection = Connection()
        with patch('connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=[[python, '-c', 'import time; time.sleep(60)']]):
            connection.start({'settings_version': 1}, 'fake-key')
            self.assertEqual(connection.events.get(timeout=5)[0], 'status')
            connection.stop()
            connection.thread.join(timeout=5)
        self.assertFalse(connection.thread.is_alive())
        self.assertIsNone(connection.job)

    def test_tray_hide_restore_and_exit(self):
        window = tk.Tk()
        settings = load_settings(Path('nonexistent-test-settings.json'))
        store = Mock()
        store.load.return_value = 'fake-saved-key'
        app = App(window, settings, store)
        try:
            self.assertEqual(app.key.get(), 'fake-saved-key')
            window.update()
            app.tray.pump()
            app.tray.update('本機煙霧測試')
            window.withdraw()
            window.update()
            self.assertEqual(window.state(), 'withdrawn')
            app.key.set('fake-key-retained')
            with patch('tray_windows.user.TrackPopupMenuEx', return_value=0) as popup:
                app.popup()
                self.assertTrue(popup.called)
            self.assertEqual(window.state(), 'withdrawn')
            self.assertEqual(app.key.get(), 'fake-key-retained')
            with patch('tray_windows.user.TrackPopupMenuEx', return_value=2), patch.object(app, 'start') as start:
                app.popup()
                start.assert_called_once()
            with patch('tray_windows.user.TrackPopupMenuEx', return_value=5), patch.object(app, 'quit') as quit_app:
                app.popup()
                quit_app.assert_called_once()
            app.show()
            window.update()
            self.assertEqual(window.state(), 'normal')
            self.assertFalse(app.running)
            app.tabs.select(1)
            window.update()
            app.reader_numbers['max_file_bytes'].set('0.5')
            app.reader_numbers['max_scan_files'].set('42')
            app.reader_lists['extensions'].delete('1.0', 'end')
            app.reader_lists['extensions'].insert('1.0', '.txt，.custom\n.md')
            collected = app.collect_advanced()
            self.assertEqual(collected['max_file_bytes'], 524288)
            self.assertEqual(collected['max_scan_files'], 42)
            self.assertEqual(collected['extensions'], ['.custom', '.md', '.txt'])
            self.assertEqual(app.key.get(), 'fake-key-retained')
            app.set_maximums()
            maximums = app.collect_advanced()
            self.assertEqual(maximums['max_file_bytes'], 64 * 1024 * 1024)
            self.assertEqual(maximums['max_scan_files'], 100000)
            self.assertEqual(maximums['max_scan_entries'], 300000)
            self.assertEqual(maximums['max_scan_bytes'], 1024 * 1024 * 1024)
            self.assertEqual(maximums['extensions'], collected['extensions'])
            self.assertTrue(app.persist_key())
            store.save.assert_called_once_with('fake-key-retained')
            app.reader_numbers['max_scan_files'].set('1.5')
            with self.assertRaises(ValueError):
                app.collect_advanced()
        finally:
            app.quit()

    def test_job_closes_entire_owned_process_tree(self):
        signature(kernel, 'OpenProcess', wintypes.HANDLE, wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)
        signature(kernel, 'WaitForSingleObject', wintypes.DWORD, wintypes.HANDLE, wintypes.DWORD)
        python = str(Path('.venv/Scripts/python.exe').resolve())
        with tempfile.TemporaryDirectory() as directory:
            pid_file = Path(directory) / 'child.pid'
            program = (
                'import subprocess,sys,time; from pathlib import Path; '
                'sys.stdin.buffer.read(1); '
                'p=subprocess.Popen([sys.executable,"-c","import time; time.sleep(60)"]); '
                'Path(sys.argv[1]).write_text(str(p.pid)); time.sleep(60)')
            job = Job()
            process = subprocess.Popen([sys._base_executable, '-c', program, str(pid_file)],
                                       stdin=subprocess.PIPE, creationflags=subprocess.CREATE_NO_WINDOW)
            handle = None
            try:
                job.assign(process)
                process.stdin.write(b'1')
                process.stdin.close()
                deadline = time.monotonic() + 5
                while not pid_file.exists() and time.monotonic() < deadline:
                    time.sleep(0.05)
                self.assertTrue(pid_file.exists())
                handle = kernel.OpenProcess(0x100000, False, int(pid_file.read_text()))
                self.assertTrue(handle)
                job.close()
                process.wait(timeout=5)
                self.assertEqual(kernel.WaitForSingleObject(handle, 5000), 0)
            finally:
                job.close()
                if process.poll() is None:
                    process.kill()
                    process.wait()
                if handle:
                    kernel.CloseHandle(handle)


if __name__ == '__main__':
    unittest.main()
