"""MCP-Local 繁體中文設定介面；金鑰以 Windows 使用者加密保存。"""
from __future__ import annotations

import ctypes
import json
import hashlib
import queue
import re
import threading
import time
from gui_tasks import GuiTasks
from power_policy import PowerPolicyManager, PowerMode, normalize_power, tick_interval
from workspace_settings import create_workspace_root
from connection_settings import load_for_edit, read_settings_data
from autostart_windows import (BackgroundStatus, apply_settings_transaction, get_background_status,
                               stop_background)
from decimal import Decimal, InvalidOperation
import tkinter as tk
from tkinter import filedialog, messagebox, ttk

from local_files_mcp import FileReader
from reader_settings import MIB, NUMERIC_FIELDS, LIST_FIELDS, normalize_reader_settings, encode_reader_settings

from connection_settings import (PROJECT, CONFIG_DIR, CONFIG_FILE, DEFAULT_TUNNEL,
                                 validate_settings, normalize_connection, load_settings, save_settings, build_commands,
                                 require_migration, backup_connection_profile)


from connection_runtime import Connection, existing_tunnel, RetryPolicy


class App:
    def __init__(self, window: tk.Tk, settings: dict, key_store=None, *, startup=False):
        from tray_windows import Tray
        self.window = window
        self.startup = startup
        self.retry_enabled = startup and settings.get("auto_connect", False)
        self.retry_policy = RetryPolicy()
        self.retry_timer = None
        self.settings = settings
        self.key_store = key_store
        self.key_save_timer = None
        self.saved_key = ''
        self.key_load_error = None
        if key_store:
            try:
                self.saved_key = key_store.load()
            except Exception:
                self.key_load_error = '無法讀取已保存的金鑰，原檔已保留；請重新輸入金鑰。'
        self.finished = False
        self.tick_timer = None
        self.wake_event = threading.Event()
        self.bridge_closed = threading.Event()
        self.tasks = GuiTasks(self.wake_event.set)
        self.power = PowerPolicyManager(CONFIG_DIR / 'power-runtime.json', options=settings.get('power'))
        self.power_mode = tk.StringVar(value='off')
        self.power_status = tk.StringVar(value='電源模式：關閉')
        self.dirty = False
        self.key_dirty = False
        self.suppress_dirty = True
        self.edit_generation = 0
        self.background_status = None
        self.state = 'IDLE'
        self.repair_required = False
        self.settings_revision = None
        self.intent = 0
        self.connection = Connection(power=self.power, notify=self.wake_event.set)
        self.running = False
        self.quitting = False
        self.restart_pending = False
        window.title('MCP-Local｜連線與快捷設定')
        window.geometry('800x820')
        window.minsize(780, 800)
        window.option_add('*Font', ('Microsoft JhengHei UI', 10))
        outer = ttk.Frame(window, padding=12)
        outer.pack(fill='both', expand=True)
        self.tabs = ttk.Notebook(outer)
        self.tabs.pack(fill='both', expand=True)
        frame = ttk.Frame(self.tabs, padding=16)
        self.tabs.add(frame, text='連線設定')
        advanced = ttk.Frame(self.tabs, padding=16)
        self.tabs.add(advanced, text='進階讀取設定')
        self.create_advanced(advanced)
        frame.columnconfigure(0, weight=1)
        ttk.Label(frame, text='本機檔案唯讀連線', font=('Microsoft JhengHei UI', 16, 'bold')).grid(
            row=0, column=0, sticky='w', pady=(0, 14))
        self.create_folder_selector(frame)
        ttk.Label(frame, text='通道識別碼').grid(row=3, column=0, sticky='w')
        self.tunnel = tk.StringVar(value=settings['tunnel'])
        ttk.Entry(frame, textvariable=self.tunnel).grid(row=4, column=0, columnspan=2, sticky='ew', pady=(4, 10))
        ttk.Label(frame, text='API 金鑰（按儲存設定後加密保存；清空輸入不會刪除舊金鑰）').grid(row=5, column=0, sticky='w')
        self.key = tk.StringVar(value=self.saved_key)
        ttk.Entry(frame, textvariable=self.key, show='●').grid(row=6, column=0, columnspan=2, sticky='ew', pady=(4, 10))
        self.hidden = tk.BooleanVar(value=settings['start_hidden'])
        ttk.Checkbutton(frame, text='下次開啟時直接縮到系統匣', variable=self.hidden).grid(row=7, column=0, sticky='w')
        ttk.Label(frame, text='僅分享你確認可公開給此連線的專用資料夾。',
                  foreground='#555555').grid(row=8, column=0, columnspan=2, sticky='w', pady=12)
        self.auto_start = tk.BooleanVar(value=settings.get('auto_start', False))
        ttk.Label(frame, text='Windows 登入自啟動').grid(row=9, column=0, sticky='w')
        ttk.Checkbutton(frame, text='Windows 登入後啟動到系統匣並自動連線', variable=self.auto_start).grid(
            row=10, column=0, columnspan=2, sticky='w')
        ttk.Label(frame, text='勾選或取消後，按「儲存設定」才會生效。\n'
                  '登入時只顯示系統匣圖示；左鍵開啟設定，右鍵管理連線或退出。\n'
                  '取消自啟動只影響下次登入，不會停止目前的系統匣連線。').grid(
                      row=11, column=0, columnspan=2, sticky='w')
        self.auto_start_status = tk.StringVar(value='登入自啟動：尚未檢查')
        ttk.Label(frame, textvariable=self.auto_start_status).grid(row=12, column=0, sticky='w')
        self.background_check_button = ttk.Button(frame, text='更新自啟動狀態', command=self.check_background)
        self.background_check_button.grid(row=13, column=0, sticky='w', pady=(6, 0))
        buttons = ttk.Frame(outer)
        buttons.pack(fill='x', pady=(12, 0))
        self.save_button = ttk.Button(buttons, text='儲存設定', command=self.save)
        self.save_button.pack(side='left', padx=(0, 6))
        self.start_button = ttk.Button(buttons, text='啟動連線', command=self.start)
        self.start_button.pack(side='left', padx=6)
        self.apply_button = ttk.Button(buttons, text='儲存並重連', command=self.restart)
        self.apply_button.pack(side='left', padx=6)
        self.stop_button = ttk.Button(buttons, text='停止連線', command=self.stop, state='disabled')
        self.stop_button.pack(side='left', padx=6)
        power_frame = ttk.LabelFrame(outer, text='電源模式', padding=6)
        power_frame.pack(fill='x', pady=(8, 0))
        self.power_buttons = []
        for mode, label in (('off', '關閉'), ('extreme', '極致節能')):
            button = ttk.Radiobutton(power_frame, text=label, variable=self.power_mode, value=mode,
                                     command=lambda value=mode: self.change_power_mode(value))
            button.pack(side='left', padx=5)
            self.power_buttons.append(button)
        ttk.Label(outer, textvariable=self.power_status, wraplength=710).pack(fill='x')
        self.status = tk.StringVar(value='尚未啟動；請確認資料夾並輸入金鑰。')
        self.suppress_dirty = False
        for variable in (self.tunnel, self.hidden, self.auto_start, *self.reader_numbers.values()):
            variable.trace_add('write', self.mark_dirty)
        for editor in self.reader_lists.values():
            editor.edit_modified(False)
            editor.bind('<<Modified>>', self.list_modified)
        self.refresh_controls()
        self.key.trace_add('write', self.schedule_key_save)
        ttk.Label(outer, textvariable=self.status, wraplength=710).pack(fill='x', pady=(10, 0))
        self.tray = Tray(lambda: window.after(0, self.show), lambda: window.after(0, self.popup), self.wake_event.set)
        window.protocol('WM_DELETE_WINDOW', self.hide)
        window.bind('<<MCPWake>>', lambda event: self.wake_tick())
        self.start_event_bridge()
        window.bind('<Control-s>', lambda event: self.save())
        window.bind('<Unmap>', self.minimized)
        self.tick_timer = window.after(100, self.tick)
        if startup or settings['start_hidden']:
            window.withdraw()
        if settings.get('auto_connect') and (startup or not settings.get('auto_start')):
            window.after(0, self.start)
        if self.key_load_error:
            window.after(0, lambda: messagebox.showerror('金鑰讀取失敗', self.key_load_error, parent=window))

    def create_advanced(self, frame) -> None:
        """集中編輯容量、掃描限制、格式及名稱排除規則。"""
        frame.columnconfigure(1, weight=1)
        self.reader_numbers = {}
        self.reader_lists = {}
        for row, (key, (label, _, maximum)) in enumerate(NUMERIC_FIELDS.items()):
            unit = MIB if key.endswith('_bytes') else 1
            text = label + ('（MiB）' if unit == MIB else '（個）')
            ttk.Label(frame, text=text).grid(row=row, column=0, sticky='w', pady=5)
            variable = tk.StringVar()
            self.reader_numbers[key] = variable
            ttk.Spinbox(frame, textvariable=variable, from_=1 / unit, to=maximum / unit,
                        increment=1, width=14).grid(row=row, column=1, sticky='w', padx=10)
            ttk.Label(frame, text=f'最高 {maximum // unit:,}').grid(row=row, column=2, sticky='w')
        labels = {
            'extensions': '允許副檔名，例如 .txt、.md、.py',
            'text_names': '允許完整檔名，例如 README、Dockerfile',
            'excluded_names': '額外排除資料夾／檔名（固定安全與產物排除不可取消）',
        }
        for index, key in enumerate(LIST_FIELDS):
            row = 4 + index * 2
            ttk.Label(frame, text=labels[key]).grid(row=row, column=0, columnspan=3, sticky='w', pady=(10, 3))
            editor = tk.Text(frame, height=3, width=50, wrap='word', undo=True)
            editor.grid(row=row + 1, column=0, columnspan=3, sticky='ew')
            self.reader_lists[key] = editor
        ttk.Label(frame, text='清單以逗號、分號或換行分隔；不使用路徑或萬用字元。\n'
                  '仍限 UTF-8 文字；隱藏路徑、連結、路徑跳脫與寫入功能維持封鎖。',
                  wraplength=680).grid(row=10, column=0, columnspan=3, sticky='w', pady=10)
        actions = ttk.Frame(frame)
        actions.grid(row=11, column=0, columnspan=3, sticky='w')
        ttk.Button(actions, text='最大值', command=self.set_maximums).pack(side='left', padx=(0, 10))
        ttk.Button(actions, text='還原進階預設值', command=lambda: self.fill_advanced(
            normalize_reader_settings())).pack(side='left')
        self.fill_advanced(normalize_reader_settings(self.settings.get('reader')))

    def set_maximums(self) -> None:
        for key, (_, _, maximum) in NUMERIC_FIELDS.items():
            divisor = MIB if key.endswith('_bytes') else 1
            self.reader_numbers[key].set(str(maximum // divisor))
        self.status.set('四項上限已設為最大值；請按「儲存設定」或「儲存並重連」。')

    def schedule_key_save(self, *args) -> None:
        self.key_dirty = True
        self.refresh_controls()
        self.status.set('金鑰輸入尚未保存；請按儲存設定。清空欄位不會刪除已保存金鑰。')

    def persist_key(self) -> bool:
        if self.key_save_timer:
            self.window.after_cancel(self.key_save_timer)
            self.key_save_timer = None
        key = self.key.get().strip()
        if self.key_store and key and key != self.saved_key:
            try:
                self.key_store.save(key)
                self.saved_key = key
            except Exception:
                self.show()
                messagebox.showerror('金鑰保存失敗', '無法加密保存 API 金鑰，請確認本機儲存空間與權限。金鑰仍保留在介面中。', parent=self.window)
                return False
        return True

    def fill_advanced(self, settings: dict) -> None:
        for key, variable in self.reader_numbers.items():
            divisor = MIB if key.endswith('_bytes') else 1
            variable.set(format(Decimal(settings[key]) / divisor, 'f'))
        for key, editor in self.reader_lists.items():
            editor.delete('1.0', 'end')
            editor.insert('1.0', ', '.join(settings[key]))

    def collect_advanced(self) -> dict:
        settings = {}
        for key, variable in self.reader_numbers.items():
            try:
                number = Decimal(variable.get().strip())
                if key.endswith('_bytes'):
                    number *= MIB
                if not number.is_finite() or number != number.to_integral_value():
                    raise ValueError()
                settings[key] = int(number)
            except (ValueError, InvalidOperation, OverflowError):
                raise ValueError(f'{NUMERIC_FIELDS[key][0]}請輸入有效數字；數量須為整數。') from None
        for key, editor in self.reader_lists.items():
            settings[key] = [part.strip() for part in re.split(r'[,;，；\n]+', editor.get('1.0', 'end')) if part.strip()]
        return normalize_reader_settings(settings)

    def minimized(self, event) -> None:
        if event.widget == self.window and self.window.state() == 'iconic':
            self.hide()

    def show(self) -> None:
        self.window.deiconify()
        self.window.lift()
        self.power.set_window_visible(True)
        self.wake_tick()

    def popup(self) -> None:
        self.tray.popup([
            ('開啟快捷設定', True, self.show),
            ('啟動連線', self.control_states()['start'], self.start),
            ('停止連線', self.control_states()['stop'], self.stop),
            ('', False, None),
            *[(label, not self.quitting and not self.tasks.busy,
               lambda value=mode: self.change_power_mode(value), self.power.mode.value == mode)
              for mode, label in (('off', '電源：關閉'), ('extreme', '電源：極致節能'))],
            ('退出程式', not self.quitting, self.quit),
        ])

    def create_folder_selector(self, parent) -> None:
        frame = ttk.LabelFrame(parent, text='允許讀取的資料夾', padding=6)
        frame.grid(row=1, column=0, columnspan=2, sticky='ew', pady=(0, 10))
        self.workspace_rows = [dict(row) for row in self.settings.get('roots', [
            {'id': 'main', 'name': 'main', 'path': self.settings['root'], 'excluded_names': []}])]
        self.invalid_root_ids = set()
        self.folder_list = tk.Listbox(frame, height=4, selectmode='extended', exportselection=False)
        self.folder_list.pack(fill='x')
        scroll = ttk.Scrollbar(frame, orient='horizontal', command=self.folder_list.xview)
        scroll.pack(fill='x')
        self.folder_list.configure(xscrollcommand=scroll.set)
        actions = ttk.Frame(frame)
        actions.pack(fill='x', pady=4)
        self.folder_buttons = []
        for label, callback in [('加入資料夾…', self.add_folder), ('變更選取…', self.change_selected_folder),
                                ('移除選取', self.remove_selected_folders)]:
            button = ttk.Button(actions, text=label, command=callback)
            button.pack(side='left', padx=(0, 6))
            self.folder_buttons.append(button)
        self.folder_status = tk.StringVar()
        ttk.Label(frame, textvariable=self.folder_status, wraplength=690).pack(anchor='w')
        self.refresh_folder_list()

    def refresh_folder_list(self) -> None:
        self.folder_list.delete(0, 'end')
        for row in self.workspace_rows:
            self.folder_list.insert('end', row['path'] + (' （無法使用）' if row['id'] in self.invalid_root_ids else ''))
        count = len(self.workspace_rows)
        self.folder_status.set(f'已加入 {count} 個資料夾（最多 8 個）；清單皆為唯讀授權範圍。' if count
                               else '至少需要加入一個允許讀取的資料夾。')

    def edit_folder(self, index=None) -> None:
        if self.tasks.busy or self.quitting:
            return
        folder = filedialog.askdirectory(parent=self.window, title='選擇允許讀取的專用資料夾', mustexist=True)
        if not folder:
            return
        try:
            previous = self.workspace_rows[index] if index is not None else None
            row = create_workspace_root(folder, self.workspace_rows, self.collect_advanced(), previous=previous)
            if index is None:
                self.workspace_rows.append(row)
            else:
                self.workspace_rows[index] = row
            self.invalid_root_ids.discard(row['id'])
            self.refresh_folder_list()
            self.mark_dirty()
        except (OSError, ValueError) as exc:
            messagebox.showerror('無法更新資料夾', str(exc), parent=self.window)

    def add_folder(self) -> None:
        self.edit_folder()

    def change_selected_folder(self) -> None:
        selection = self.folder_list.curselection()
        if len(selection) == 1:
            self.edit_folder(selection[0])

    def remove_selected_folders(self) -> None:
        if self.tasks.busy or self.quitting:
            return
        for index in reversed(self.folder_list.curselection()):
            self.invalid_root_ids.discard(self.workspace_rows[index]['id'])
            del self.workspace_rows[index]
        self.refresh_folder_list()
        self.mark_dirty()

    def hide(self) -> None:
        self.window.withdraw()
        self.power.set_window_visible(False)
        self.wake_tick()

    def start_event_bridge(self) -> None:
        # A separate Python thread marshals notifications through Tcl's thread queue.
        # Never call Tk from a ctypes WNDPROC, where Tcl may already be dispatching.
        def bridge():
            while True:
                self.wake_event.wait()
                self.wake_event.clear()
                if self.bridge_closed.is_set():
                    return
                try:
                    self.window.event_generate('<<MCPWake>>', when='tail')
                except (RuntimeError, tk.TclError):
                    return
        threading.Thread(target=bridge, name='mcp-ui-events', daemon=True).start()

    def wake_tick(self) -> None:
        if self.finished:
            return
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
        self.tick_timer = self.window.after(0, self.tick)

    def tick_interval_ms(self) -> int:
        return tick_interval(self.power.mode.value, self.window.state() != 'withdrawn', self.state,
                             self.tasks.busy, not self.connection.events.empty() or not self.tasks.events.empty())

    def change_power_mode(self, mode: str) -> None:
        self.power_mode.set(self.power.mode.value)
        if self.tasks.busy or self.quitting:
            return
        def complete(value, error):
            self.power_mode.set(self.power.mode.value)
            if error:
                self.power_status.set(str(error))
                messagebox.showerror('電源模式未套用', str(error), parent=self.window)
            else:
                self.mark_dirty()
                self.update_power_status()
            self.refresh_controls()
        self.tasks.submit(lambda: self.power.apply(mode), complete)
        self.refresh_controls()
        self.wake_tick()

    def update_power_status(self) -> None:
        label = {'off': '關閉', 'extreme': '極致節能'}[self.power.mode.value]
        status = '電源模式：' + label
        if self.power.mode != PowerMode.OFF:
            status += '｜顯示器可休眠'
            status += '｜連線保持喚醒' if self.power.request_handle else '｜等待連線'
            if (self.power.options['manage_windows_power_mode'] and
                    not any(code.startswith('POWER_USER_MODE_') for code in self.power.warnings)):
                status += '｜Windows 最佳電源效率'
        if self.power.warnings:
            status += '｜部分能力未生效：' + ', '.join(self.power.warnings)
        self.power_status.set(status)

    def mark_dirty(self, *args) -> None:
        if not self.suppress_dirty:
            self.dirty = True
            self.edit_generation += 1
            self.refresh_controls()
            self.wake_tick()

    def list_modified(self, event) -> None:
        if event.widget.edit_modified():
            event.widget.edit_modified(False)
            self.mark_dirty()

    def control_states(self) -> dict:
        available = not self.quitting and not self.tasks.busy
        stable = available and self.state in ('IDLE', 'RUNNING', 'REPAIR')
        bg = self.background_status
        return {
            'save': stable and bool(self.workspace_rows) and (self.dirty or self.key_dirty or self.repair_required),
            'start': stable and bool(self.workspace_rows) and not self.running and not self.repair_required
                     and not (bg and bg.running),
            'apply': stable and bool(self.workspace_rows) and self.running and not self.repair_required,
            'stop': available and self.state in ('IDLE', 'STARTING', 'RUNNING', 'REPAIR')
                    and (self.running or self.retry_timer is not None
                         or bool(bg and bg.running and bg.task_valid and not bg.error_code)),
            'background_check': stable,
        }

    def refresh_controls(self) -> None:
        for name, enabled in self.control_states().items():
            button = getattr(self, name + '_button')
            state = 'normal' if enabled else 'disabled'
            # 重設相同 state 會中斷 Windows 原生 hover / pressed 動畫。
            if str(button.cget('state')) != state:
                button.configure(state=state)
        enabled = not self.quitting and not self.tasks.busy
        for button in self.power_buttons + self.folder_buttons:
            state = 'normal' if enabled else 'disabled'
            if str(button.cget('state')) != state:
                button.configure(state=state)
        bg = self.background_status
        if self.dirty or self.key_dirty:
            auto = '有未儲存變更；請先儲存'
        elif bg is None:
            auto = '尚未檢查'
        elif bg.error_code:
            auto = '無法查詢'
        elif bg.registered and not bg.task_valid:
            auto = '工作設定不一致'
        elif self.settings.get('auto_start'):
            auto = '已啟用' if bg.registered else '工作遺失'
        else:
            auto = '工作設定不一致' if bg.registered else '未啟用'
        self.auto_start_status.set('登入自啟動：' + auto)

    def check_background(self) -> None:
        if not self.control_states()['background_check']:
            return
        def complete(value, error):
            self.background_status = (BackgroundStatus(False, False, False, 'STATUS_QUERY_FAILED')
                                      if error else value)
            if value and value.running:
                self.status.set('已有連線執行中；請先按「停止連線」再重新啟動。')
            self.refresh_controls()
        self.tasks.submit(get_background_status, complete)
        self.refresh_controls()

    def submit_background(self, action) -> None:
        def complete(value, error):
            self.background_status = None
            if error:
                messagebox.showerror('停止連線失敗', '無法停止連線；請更新狀態後重試。', parent=self.window)
            self.check_background()
            self.refresh_controls()
        self.tasks.submit(action, complete)
        self.refresh_controls()

    def reload_settings(self) -> None:
        editable = load_for_edit()
        self.suppress_dirty = True
        try:
            self.settings = editable.settings
            self.settings_revision = editable.revision if editable.revision is not None else ''
            self.repair_required = editable.state == 'REPAIR_REQUIRED'
            value = self.settings
            self.tunnel.set(value['tunnel'])
            self.hidden.set(value['start_hidden'])
            self.auto_start.set(value.get('auto_start', False))
            self.workspace_rows = [dict(row) for row in value['roots']]
            self.invalid_root_ids = {row['root_id'] for row in editable.errors}
            self.refresh_folder_list()
            self.fill_advanced(normalize_reader_settings(value.get('reader')))
            for editor in self.reader_lists.values():
                editor.edit_modified(False)
            self.dirty = False
        finally:
            self.suppress_dirty = False
        self.state = 'REPAIR' if self.repair_required else 'RUNNING' if self.running else 'IDLE'
        self.background_status = None
        self.refresh_controls()

    def save(self, after=None, retry_on_conflict=True) -> bool:
        if self.tasks.busy or self.quitting or self.state not in ('IDLE', 'RUNNING', 'REPAIR'):
            return False
        try:
            if not self.workspace_rows:
                raise ValueError('至少需要加入一個允許讀取的資料夾。')
            settings = {'root': self.workspace_rows[0]['path'], 'tunnel': self.tunnel.get().strip()}
            settings['recent'] = list(self.settings.get('recent', []))
            settings['power'] = {**normalize_power(self.settings.get('power')), 'mode': self.power.mode.value}
            settings['start_hidden'] = self.hidden.get()
            settings['auto_start'] = self.auto_start.get()
            settings['auto_connect'] = self.auto_start.get()
            if settings['auto_start'] and (not self.key_store or not self.key.get().strip()):
                raise ValueError('自動啟動需要已保存的 DPAPI 金鑰。')
            settings['reader'] = self.collect_advanced()
            settings['default_root'] = self.workspace_rows[0]['id']
            settings['roots'] = [dict(row) for row in self.workspace_rows]
            if self.settings.get('settings_version') not in (1, 2, 3, 4, 5):
                if not messagebox.askyesno('首次共用設定遷移',
                        '舊 PowerShell 分享範圍為 D:\\codee；GUI 使用獨立設定。\n'
                        f'確認兩個入口今後都使用：\n{settings["root"]}\n{settings["tunnel"]}\n'
                        '確認後會備份並保存一般設定；加密金鑰保留。', parent=self.window):
                    return False
            settings['settings_version'] = 5
            previous = self.settings.copy()
            key = self.key.get().strip()
            saved_key = self.saved_key
            revision = self.settings_revision
            intent = self.intent
            generation = self.edit_generation
            was_dirty = self.dirty
            save_backend = save_settings
            key_state = {'saved': False}
            def work():
                normalized = normalize_connection(settings)
                if self.key_store and key and key != saved_key:
                    self.key_store.save(key)
                    key_state['saved'] = True
                def save(value):
                    if revision is None:
                        save_backend(value)
                    else:
                        save_backend(value, expected_revision=revision)
                apply_settings_transaction(normalized, previous, save)
                return normalized
            def complete(value, error):
                if self.quitting:
                    return
                if key_state['saved']:
                    self.saved_key = key
                    self.key_dirty = self.key.get().strip() != key
                if error:
                    self.state = 'REPAIR' if self.repair_required else 'RUNNING' if self.running else 'IDLE'
                    self.refresh_controls()
                    if getattr(error, 'code', None) == 'SETTINGS_CONFLICT':
                        local_changes = was_dirty or self.dirty
                        reload = (not local_changes and retry_on_conflict) or (local_changes and messagebox.askyesno(
                            '設定已被其他操作更新', '磁碟設定已變更，本次儲存已取消。是否捨棄未儲存的一般設定並重新載入？API 金鑰輸入會保留。', parent=self.window))
                        if reload:
                            try:
                                self.reload_settings()
                                if not local_changes and intent == self.intent and not self.repair_required:
                                    self.save(after=after, retry_on_conflict=False)
                            except Exception:
                                messagebox.showerror('無法重新載入', '設定無法載入；目前編輯內容已保留。', parent=self.window)
                            return
                        if local_changes:
                            return
                    messagebox.showerror('無法儲存設定', str(error), parent=self.window)
                    return
                self.settings = value
                self.settings_revision = hashlib.sha256(json.dumps(value, ensure_ascii=False, indent=2).encode('utf-8')).hexdigest()
                self.repair_required = False
                self.state = 'RUNNING' if self.running else 'IDLE'
                if generation == self.edit_generation:
                    self.suppress_dirty = True
                    self.workspace_rows = [dict(row) for row in value['roots']]
                    self.invalid_root_ids.clear()
                    self.refresh_folder_list()
                    self.suppress_dirty = False
                    self.dirty = False
                self.key_dirty = self.key.get().strip() != key
                self.background_status = None
                self.refresh_controls()
                self.status.set('設定已儲存；新的資料夾範圍會在下次連線時使用。')
                if after and intent == self.intent and generation == self.edit_generation and not self.key_dirty:
                    after()
                elif not after:
                    self.check_background()
            self.state = 'SAVING'
            self.status.set('正在驗證與儲存設定…')
            submitted = self.tasks.submit(work, complete)
            self.refresh_controls()
            self.wake_tick()
            return submitted
        except Exception as exc:
            self.show()
            messagebox.showerror('無法儲存設定', str(exc), parent=self.window)
            return False

    def start(self) -> None:
        if self.retry_timer is not None:
            self.window.after_cancel(self.retry_timer)
            self.retry_timer = None
        if not self.control_states()['start']:
            if self.repair_required:
                self.status.set('修復模式：請先修正所有失效共享資料夾並儲存設定。')
            return
        if not self.key.get().strip():
            messagebox.showinfo('需要金鑰', '請輸入通道 API 金鑰。', parent=self.window)
            return
        if self.dirty or self.key_dirty:
            self.save(after=self.launch)
        else:
            self.reload_and_launch()

    def reload_and_launch(self) -> None:
        # 正式入口帶有 revision；啟動前重讀以免使用外部更新前的舊值。
        if self.settings_revision is not None:
            try:
                self.reload_settings()
            except Exception:
                messagebox.showerror('無法啟動', '無法載入最新設定。', parent=self.window)
                return
        self.launch()

    def launch(self) -> None:
        if self.quitting or self.running or self.repair_required:
            return
        if self.connection.thread and self.connection.thread.is_alive():
            self.window.after(50, self.launch)
            return
        # 每次連線使用獨立事件佇列，舊事件不會污染新連線。
        self.connection = Connection(power=self.power, notify=self.wake_event.set)
        try:
            self.connection.start(self.settings.copy(), self.key.get().strip())
            self.running = True
            self.state = 'STARTING'
            self.refresh_controls()
        except Exception:
            self.running = False
            self.state = 'IDLE'
            self.status.set('連線啟動失敗。')
            self.refresh_controls()

    def restart(self) -> None:
        if not self.control_states()['apply']:
            return
        def apply():
            self.restart_pending = True
            self.connection.cancel.set()
            self.state = 'RESTARTING'
            self.status.set('正在停止目前連線…')
            self.refresh_controls()
        if self.dirty or self.key_dirty:
            self.save(after=apply)
        else:
            apply()

    def stop(self) -> None:
        self.wake_tick()
        self.retry_enabled = False
        if self.retry_timer is not None:
            self.window.after_cancel(self.retry_timer)
            self.retry_timer = None
        bg = self.background_status
        if not self.running and bg and bg.running and self.control_states()['stop']:
            self.submit_background(stop_background)
        self.intent += 1
        self.restart_pending = False
        self.connection.cancel.set()
        if self.running and not self.quitting and self.state != 'SAVING':
            self.state = 'STOPPING'
            self.status.set('正在停止…')
        self.refresh_controls()

    def quit(self) -> None:
        self.quitting = True
        self.state = 'QUITTING'
        self.stop()
        if not self.running and not self.tasks.busy:
            self.finish()

    def finish(self) -> None:
        if self.finished:
            return
        try:
            self.power.close()
        except Exception as exc:
            self.quitting = False
            self.state = 'IDLE'
            self.status.set(str(exc))
            messagebox.showerror('電源還原未完成', str(exc), parent=self.window)
            self.wake_tick()
            return
        self.finished = True
        self.bridge_closed.set()
        self.wake_event.set()
        if self.retry_timer is not None:
            self.window.after_cancel(self.retry_timer)
            self.retry_timer = None
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
            self.tick_timer = None
        self.tasks.close()
        self.tray.close()
        self.window.destroy()

    def retry_connection(self) -> None:
        self.retry_timer = None
        if not self.retry_enabled or self.quitting:
            return
        if self.dirty or self.key_dirty or self.repair_required:
            self.retry_enabled = False
            self.status.set('自動重試已暫停；請確認修改內容後按「啟動連線」。')
            return
        if self.tasks.busy or self.state not in ('IDLE', 'RUNNING'):
            self.retry_timer = self.window.after(100, self.retry_connection)
            return
        if not self.running:
            self.reload_and_launch()

    def tick(self) -> None:
        self.tick_timer = None
        if self.finished:
            return
        self.tasks.poll()
        if self.quitting and not self.running and not self.tasks.busy:
            self.finish()
            return
        self.tray.pump()
        while not self.connection.events.empty():
            kind, text = self.connection.events.get_nowait()
            if kind == 'power_warning':
                if text not in self.power.warnings:
                    self.power.warnings.append(text)
                self.update_power_status()
                continue
            if kind == 'error' and self.retry_enabled and self.connection.error_kind in self.retry_policy.retryable:
                self.status.set('連線暫時失敗，將自動重試。')
                continue
            if kind == 'error':
                self.show()
                messagebox.showerror('連線未完成', text, parent=self.window)
            elif kind == 'done':
                if self.connection.thread and self.connection.thread.is_alive():
                    self.connection.events.put((kind, text))
                    break
                self.running = False
                if self.state != 'SAVING':
                    self.state = 'REPAIR' if self.repair_required else 'IDLE'
                if self.quitting:
                    self.finish()
                    return
                if self.restart_pending:
                    self.restart_pending = False
                    self.status.set('正在使用最新設定重新啟動…')
                    self.reload_and_launch()
                    break
                if self.retry_enabled and self.connection.error_kind in self.retry_policy.retryable:
                    if self.connection.running_since and time.monotonic() - self.connection.running_since >= 60:
                        self.retry_policy.reset()
                    delay = self.retry_policy.next_delay()
                    self.status.set(f'連線暫時失敗，約 {delay:.0f} 秒後重試；可按「停止連線」取消。')
                    self.retry_timer = self.window.after(round(delay * 1000), self.retry_connection)
                    break
            if kind == 'status' and text == '通道程序執行中' and self.state == 'STARTING':
                self.state = 'RUNNING'
            self.status.set(text + ('；遠端可用性請以實際工具呼叫確認。' if text == '通道程序執行中' else ''))
            self.tray.update(text)
        self.refresh_controls()
        self.update_power_status()
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
        self.tick_timer = self.window.after(self.tick_interval_ms(), self.tick)


def main(*, startup: bool = False) -> int:
    from tray_windows import kernel
    window = tk.Tk()
    window.withdraw()
    mutex = kernel.CreateMutexW(None, False, 'Local\\MCP_Local_Settings_UI')
    if not mutex:
        messagebox.showerror('無法啟動', '無法建立程式鎖定。', parent=window)
        window.destroy()
        return 1
    try:
        if ctypes.get_last_error() == 183:
            if not startup:
                messagebox.showinfo('程式已開啟', '請在右下角系統匣（可能位於隱藏圖示內）點選 MCP-Local。', parent=window)
            window.destroy()
            return 0
        editable = load_for_edit()
        settings = editable.settings
        if startup and not settings.get('auto_start'):
            window.destroy()
            return 0
        from key_store import KeyStore
        app = App(window, settings, KeyStore(CONFIG_DIR / 'api-key.dpapi'), startup=startup)
        app.power.recover()
        app.power.apply(normalize_power(settings.get('power'))['mode'])
        app.power_mode.set(app.power.mode.value)
        app.update_power_status()
        app.settings_revision = editable.revision if editable.revision is not None else ''
        app.repair_required = editable.state == 'REPAIR_REQUIRED'
        if app.repair_required:
            app.state = 'REPAIR'
            app.invalid_root_ids = {row['root_id'] for row in editable.errors}
            app.refresh_folder_list()
            app.status.set('修復模式：請變更或移除標示為無法使用的資料夾。')
        if app.repair_required or (not startup and not settings['start_hidden']):
            app.show()
        app.refresh_controls()
        window.after(0, app.check_background)
        window.mainloop()
        return 0
    except Exception as exc:
        messagebox.showerror('MCP-Local 啟動失敗', str(exc), parent=window)
        window.destroy()
        return 1
    finally:
        kernel.CloseHandle(mutex)
        if 'app' in locals():
            app.connection.stop()
            if app.connection.thread:
                app.connection.thread.join(timeout=10)
            app.power.close()


if __name__ == '__main__':
    main()
