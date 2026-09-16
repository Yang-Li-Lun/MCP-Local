"""MCP-Local 繁體中文設定介面；金鑰以 Windows 使用者加密保存。"""
from __future__ import annotations

import ctypes
import json
import queue
import re
import threading
from decimal import Decimal, InvalidOperation
import tkinter as tk
from tkinter import filedialog, messagebox, ttk

from local_files_mcp import FileReader
from reader_settings import MIB, NUMERIC_FIELDS, LIST_FIELDS, normalize_reader_settings, encode_reader_settings

from connection_settings import (PROJECT, CONFIG_DIR, CONFIG_FILE, DEFAULT_TUNNEL,
                                 validate_settings, normalize_connection, load_settings, save_settings, build_commands,
                                 require_migration, backup_connection_profile)


from connection_runtime import Connection, existing_tunnel
from autostart_windows import (register_autostart, unregister_autostart,
                               validate_registered_task, is_registered, control_background, background_running)


class App:
    def __init__(self, window: tk.Tk, settings: dict, key_store=None):
        from tray_windows import Tray
        self.window = window
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
        self.connection = Connection()
        self.running = False
        self.quitting = False
        self.restart_pending = False
        window.title('MCP-Local｜連線與快捷設定')
        window.geometry('780x700')
        window.minsize(740, 680)
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
        ttk.Label(frame, text='可讀取資料夾（下拉選單可切換最近使用的目錄）').grid(row=1, column=0, sticky='w')
        self.root = tk.StringVar(value=settings['root'])
        self.roots = ttk.Combobox(frame, textvariable=self.root, values=settings['recent'])
        self.roots.grid(row=2, column=0, sticky='ew', pady=(4, 10))
        ttk.Button(frame, text='瀏覽…', command=self.browse).grid(row=2, column=1, padx=(8, 0))
        ttk.Label(frame, text='通道識別碼').grid(row=3, column=0, sticky='w')
        self.create_workspace_tab()
        self.tunnel = tk.StringVar(value=settings['tunnel'])
        ttk.Entry(frame, textvariable=self.tunnel).grid(row=4, column=0, columnspan=2, sticky='ew', pady=(4, 10))
        ttk.Label(frame, text='API 金鑰（自動加密保存，下次開啟自動帶入）').grid(row=5, column=0, sticky='w')
        self.key = tk.StringVar(value=self.saved_key)
        ttk.Entry(frame, textvariable=self.key, show='●').grid(row=6, column=0, columnspan=2, sticky='ew', pady=(4, 10))
        self.hidden = tk.BooleanVar(value=settings['start_hidden'])
        ttk.Checkbutton(frame, text='下次開啟時直接縮到系統匣', variable=self.hidden).grid(row=7, column=0, sticky='w')
        ttk.Label(frame, text='僅分享你確認可公開給此連線的專用資料夾。\n關閉視窗會縮到系統匣；從右鍵選單「結束程式」才會停止並退出。',
                  foreground='#555555').grid(row=8, column=0, columnspan=2, sticky='w', pady=12)
        self.auto_start = tk.BooleanVar(value=settings.get('auto_start', False))
        ttk.Checkbutton(frame, text='Windows 登入後自動啟動並連線', variable=self.auto_start).grid(
            row=9, column=0, columnspan=2, sticky='w')
        self.auto_status = tk.StringVar(value='自動啟動狀態尚未查詢')
        ttk.Label(frame, textvariable=self.auto_status).grid(row=10, column=0, sticky='w')
        background = ttk.Frame(frame)
        background.grid(row=11, column=0, columnspan=2, sticky='w')
        for label, callback in [('檢查背景狀態', self.check_background),
                                ('停止背景連線', lambda: self.background_action(False)),
                                ('套用並重連背景', lambda: self.background_action(True))]:
            ttk.Button(background, text=label, command=callback).pack(side='left', padx=3)
        buttons = ttk.Frame(outer)
        buttons.pack(fill='x', pady=(12, 0))
        ttk.Button(buttons, text='儲存設定', command=self.save).pack(side='left', padx=(0, 6))
        self.start_button = ttk.Button(buttons, text='啟動連線', command=self.start)
        self.start_button.pack(side='left', padx=6)
        self.apply_button = ttk.Button(buttons, text='套用並重新連線', command=self.restart)
        self.apply_button.pack(side='left', padx=6)
        self.stop_button = ttk.Button(buttons, text='停止', command=self.stop, state='disabled')
        self.stop_button.pack(side='left', padx=6)
        ttk.Button(buttons, text='縮到系統匣', command=window.withdraw).pack(side='left', padx=6)
        self.status = tk.StringVar(value='尚未啟動；請確認資料夾並輸入金鑰。')
        self.key.trace_add('write', self.schedule_key_save)
        ttk.Label(outer, textvariable=self.status, wraplength=710).pack(fill='x', pady=(10, 0))
        self.tray = Tray(lambda: window.after(0, self.show), lambda: window.after(0, self.popup))
        window.protocol('WM_DELETE_WINDOW', window.withdraw)
        window.bind('<Control-s>', lambda event: self.save())
        window.bind('<Unmap>', self.minimized)
        window.after(100, self.tick)
        if settings['start_hidden']:
            window.withdraw()
        if settings.get('auto_connect') and not settings.get('auto_start'):
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
        self.status.set('四項上限已設為最大值；請儲存設定或套用並重新連線。')

    def schedule_key_save(self, *args) -> None:
        if self.key_save_timer:
            self.window.after_cancel(self.key_save_timer)
        self.key_save_timer = self.window.after(700, self.persist_key)

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
            self.window.withdraw()

    def show(self) -> None:
        self.window.deiconify()
        self.window.lift()

    def popup(self) -> None:
        self.tray.popup([
            ('開啟快捷設定', True, self.show),
            ('啟動連線', not self.running and not self.quitting, self.start),
            ('停止連線', self.running and not self.quitting, self.stop),
            ('', False, None),
            ('退出程式', not self.quitting, self.quit),
        ])

    def create_workspace_tab(self) -> None:
        """管理明確授權的具名資料夾，不自動加入最近路徑。"""
        frame = ttk.Frame(self.tabs, padding=16)
        self.tabs.add(frame, text='共享資料夾')
        ttk.Label(frame, text='最多八個資料夾；連線設定頁的路徑對應此處選定的預設資料夾。').pack(anchor='w')
        self.workspace_rows = [dict(item) for item in self.settings.get('roots', [
            {'id': 'main', 'name': 'main', 'path': self.settings['root'], 'excluded_names': []}])]
        self.workspace_default = tk.StringVar(value=self.settings.get('default_root', 'main'))
        self.workspace_list = tk.Listbox(frame, height=8)
        self.workspace_list.pack(fill='x', pady=8)
        self.workspace_list.bind('<<ListboxSelect>>', self.select_workspace_row)
        self.workspace_fields = {}
        for key, label in [('id', '代號'), ('name', '名稱'), ('path', '絕對資料夾路徑'),
                           ('excluded_names', '額外排除名稱（逗號分隔）')]:
            ttk.Label(frame, text=label).pack(anchor='w')
            variable = tk.StringVar()
            self.workspace_fields[key] = variable
            ttk.Entry(frame, textvariable=variable).pack(fill='x')
        actions = ttk.Frame(frame)
        actions.pack(fill='x', pady=8)
        ttk.Button(actions, text='瀏覽', command=self.browse_workspace).pack(side='left')
        ttk.Button(actions, text='加入／更新代號', command=self.update_workspace_row).pack(side='left', padx=6)
        ttk.Button(actions, text='移除選取', command=self.remove_workspace_row).pack(side='left')
        ttk.Label(frame, text='預設資料夾代號').pack(anchor='w')
        self.workspace_choice = ttk.Combobox(frame, textvariable=self.workspace_default, state='readonly')
        self.workspace_choice.pack(fill='x')
        self.workspace_choice.bind('<<ComboboxSelected>>', self.change_workspace_default)
        self.refresh_workspace_rows()

    def refresh_workspace_rows(self) -> None:
        self.workspace_list.delete(0, 'end')
        for item in self.workspace_rows:
            self.workspace_list.insert('end', item['id'] + ' | ' + item['name'] + ' | ' + item['path'])
        self.workspace_choice.configure(values=[item['id'] for item in self.workspace_rows])

    def select_workspace_row(self, event=None) -> None:
        selection = self.workspace_list.curselection()
        if selection:
            item = self.workspace_rows[selection[0]]
            for key, variable in self.workspace_fields.items():
                variable.set(', '.join(item.get(key, [])) if key == 'excluded_names' else item[key])

    def browse_workspace(self) -> None:
        folder = filedialog.askdirectory(parent=self.window, title='選擇新增的專用共享資料夾', mustexist=True)
        if folder:
            self.workspace_fields['path'].set(folder)

    def update_workspace_row(self) -> None:
        from workspace_settings import normalize_workspace
        try:
            item = {key: variable.get().strip() for key, variable in self.workspace_fields.items()}
            item['excluded_names'] = [part.strip() for part in item['excluded_names'].split(',') if part.strip()]
            rows = [dict(row) for row in self.workspace_rows]
            existing = next((i for i, row in enumerate(rows) if row['id'] == item['id']), None)
            if existing is None:
                rows.append(item)
            else:
                rows[existing] = item
            normalized = normalize_workspace(rows, self.workspace_default.get(), self.collect_advanced())
            self.workspace_rows = normalized['roots']
            self.change_workspace_default()
            self.refresh_workspace_rows()
        except (ValueError, OSError) as exc:
            messagebox.showerror('無法更新資料夾', str(exc), parent=self.window)

    def remove_workspace_row(self) -> None:
        selection = self.workspace_list.curselection()
        if not selection:
            return
        item = self.workspace_rows[selection[0]]
        if item['id'] == self.workspace_default.get():
            messagebox.showerror('無法移除', '請先選擇其他預設資料夾。', parent=self.window)
            return
        del self.workspace_rows[selection[0]]
        self.refresh_workspace_rows()

    def change_workspace_default(self, event=None) -> None:
        item = next(row for row in self.workspace_rows if row['id'] == self.workspace_default.get())
        self.root.set(item['path'])

    def browse(self) -> None:
        folder = filedialog.askdirectory(parent=self.window, title='選擇允許讀取的專用資料夾', mustexist=True)
        if folder:
            self.root.set(folder)

    def check_background(self) -> None:
        def check():
            try:
                registered = is_registered()
                valid = registered and validate_registered_task()
                text = ('自動啟動：已啟用' if valid and self.settings.get('auto_start') else
                        '自動啟動：設定不一致' if registered else
                        '自動啟動：工作排程遺失' if self.settings.get('auto_start') else '自動啟動：未啟用')
                text += '；背景程序執行中' if background_running() else '；背景程序未執行'
                text += '；通道程序存在' if existing_tunnel() else '；未偵測到通道程序'
            except Exception:
                text = '無法查詢背景狀態。'
            self.window.after(0, lambda: self.auto_status.set(text))
        threading.Thread(target=check, daemon=True).start()

    def background_action(self, restart: bool) -> None:
        if restart and not self.save():
            return
        try:
            control_background(restart)
            self.check_background()
        except Exception as exc:
            messagebox.showerror('背景連線', str(exc), parent=self.window)

    def save(self) -> bool:
        try:
            settings = validate_settings(self.root.get().strip(), self.tunnel.get().strip())
            settings['recent'] = list(dict.fromkeys([settings['root'], *self.settings['recent']]))[:8]
            settings['start_hidden'] = self.hidden.get()
            settings['auto_start'] = self.auto_start.get()
            settings['auto_connect'] = self.auto_start.get()
            if settings['auto_start'] and (not self.key_store or not self.key.get().strip()):
                raise ValueError('自動啟動需要已保存的 DPAPI 金鑰。')
            settings['reader'] = self.collect_advanced()
            settings['default_root'] = self.workspace_default.get()
            settings['roots'] = [dict(row) for row in self.workspace_rows]
            for row in settings['roots']:
                if row['id'] == settings['default_root']:
                    row['path'] = settings['root']
            if self.settings.get('settings_version') not in (1, 2, 3):
                if not messagebox.askyesno('首次共用設定遷移',
                        '舊 PowerShell 分享範圍為 D:\\codee；GUI 使用獨立設定。\n'
                        f'確認兩個入口今後都使用：\n{settings["root"]}\n{settings["tunnel"]}\n'
                        '確認後會備份並保存一般設定；加密金鑰保留。', parent=self.window):
                    return False
            settings['settings_version'] = 3
            settings = normalize_connection(settings)
            if not self.persist_key():
                return False
            previous = self.settings.copy()
            save_settings(settings)
            try:
                if settings['auto_start']:
                    register_autostart()
                elif previous.get('auto_start'):
                    unregister_autostart()
            except Exception:
                save_settings(previous)
                raise
            self.settings = settings
            self.workspace_rows = [dict(row) for row in settings['roots']]
            self.refresh_workspace_rows()
            self.root.set(settings['root'])
            self.roots.configure(values=settings['recent'])
            self.status.set('設定已儲存。' + ('連線仍使用原設定，請按「套用並重新連線」。' if self.running else '可啟動連線。'))
            return True
        except Exception as exc:
            self.show()
            messagebox.showerror('無法儲存設定', str(exc), parent=self.window)
            return False

    def start(self) -> None:
        if self.running or self.quitting:
            return
        if not self.key.get().strip():
            self.show()
            messagebox.showinfo('需要金鑰', '請輸入通道執行階段 API 金鑰，之後會自動加密保存。', parent=self.window)
            return
        if not self.save():
            return
        self.running = True
        self.start_button.configure(state='disabled')
        self.stop_button.configure(state='normal')
        self.status.set('正在啟動，共享資料夾代號：' + ', '.join(row['id'] for row in self.settings['roots']))
        self.connection.start(self.settings.copy(), self.key.get().strip())

    def restart(self) -> None:
        if not self.save():
            return
        if self.running:
            if not self.key.get().strip():
                self.show()
                messagebox.showinfo('需要金鑰', '請先輸入金鑰，再套用並重新連線。', parent=self.window)
                return
            self.restart_pending = True
            self.connection.stop()
            self.status.set('正在停止舊連線，稍後套用新設定…')
        else:
            self.start()

    def stop(self) -> None:
        self.restart_pending = False
        self.connection.stop()
        if self.running:
            self.status.set('正在停止…')

    def quit(self) -> None:
        if not self.persist_key():
            return
        self.quitting = True
        self.stop()
        if not self.running:
            self.finish()

    def finish(self) -> None:
        self.tray.close()
        self.window.destroy()

    def tick(self) -> None:
        self.tray.pump()
        while not self.connection.events.empty():
            kind, text = self.connection.events.get_nowait()
            if kind == 'error':
                self.show()
                messagebox.showerror('連線未完成', text, parent=self.window)
            elif kind == 'done':
                self.running = False
                self.start_button.configure(state='normal')
                self.stop_button.configure(state='disabled')
                if self.quitting:
                    self.finish()
                    return
                if self.restart_pending:
                    self.restart_pending = False
                    self.start()
                    break
            self.status.set(text + ('；遠端可用性請以實際工具呼叫確認。' if text == '通道程序執行中' else ''))
            self.tray.update(text)
        self.window.after(100, self.tick)


def main() -> None:
    from tray_windows import kernel
    window = tk.Tk()
    window.withdraw()
    mutex = kernel.CreateMutexW(None, False, 'Local\\MCP_Local_Settings_UI')
    if not mutex:
        messagebox.showerror('無法啟動', '無法建立程式鎖定。', parent=window)
        window.destroy()
        return
    try:
        if ctypes.get_last_error() == 183:
            messagebox.showinfo('程式已開啟', '請在右下角系統匣（可能位於隱藏圖示內）點選 MCP-Local。', parent=window)
            return
        settings = load_settings()
        from key_store import KeyStore
        app = App(window, settings, KeyStore(CONFIG_DIR / 'api-key.dpapi'))
        if not settings['start_hidden']:
            app.show()
        window.mainloop()
    except Exception as exc:
        messagebox.showerror('MCP-Local 啟動失敗', str(exc), parent=window)
    finally:
        kernel.CloseHandle(mutex)
        if 'app' in locals():
            app.connection.stop()


if __name__ == '__main__':
    main()
