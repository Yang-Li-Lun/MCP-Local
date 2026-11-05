"""WPF 桌面的 Python 設定交易與連線狀態機；不依賴 Tkinter。"""
from __future__ import annotations
import hashlib
import json
import re
import threading
import time
from decimal import Decimal, InvalidOperation
from mcp_local.desktop_ports import Value, TextValue, Scheduler, Dialogs, DesktopWake
from mcp_local.gui_tasks import GuiTasks
from mcp_local.icon_assets import icon_path
from mcp_local.workspace_settings import create_workspace_root
from mcp_local.connection_settings import (CONFIG_DIR, SETTINGS_VERSION, load_for_edit,
    normalize_connection, save_settings)
from mcp_local.reader_settings import MIB, NUMERIC_FIELDS, LIST_FIELDS, normalize_reader_settings
from mcp_local.autostart_windows import (BackgroundStatus, apply_settings_transaction,
    get_background_status, stop_background)
from mcp_local.connection_runtime import Connection, RetryPolicy
from mcp_local.request_monitor import Monitor

messagebox = Dialogs()


class DesktopController:
    def __init__(self, window: Scheduler, settings: dict, key_store=None, *, startup=False,
                 dialogs=None, tray_factory=None):
        from mcp_local.tray_windows import Tray
        self.window, self.settings, self.key_store = window, settings, key_store
        self.dialogs = dialogs or messagebox
        self.startup = startup
        self.retry_enabled = startup and settings.get('auto_connect', False)
        self.retry_policy = RetryPolicy()
        self.retry_timer = self.key_save_timer = self.tick_timer = None
        self.saved_key = ''
        self.key_load_error = None
        if key_store:
            try:
                self.saved_key = key_store.load()
            except Exception:
                self.key_load_error = '無法讀取已保存的金鑰，原檔已保留；請重新輸入金鑰。'
        self.finished = self.quitting = self.running = self.restart_pending = False
        self.wake_event = DesktopWake()
        self.tasks = GuiTasks(self.wake_event.set)
        self.suppress_dirty = True
        self.dirty = self.key_dirty = self.repair_required = False
        self.edit_generation = self.intent = 0
        self.settings_revision = self.background_status = None
        self.state = 'IDLE'
        self.active_access_mode = None
        self.request_monitor = Monitor()
        self.connection = Connection(notify=self.wake_event.set, monitor=self.request_monitor)
        self.tunnel = Value(settings['tunnel'], self.mark_dirty)
        self.hidden = Value(settings['start_hidden'], self.mark_dirty)
        self.auto_start = Value(settings.get('auto_start', False), self.mark_dirty)
        self.icon_style = Value(settings.get('icon_style', 'classic'), self.mark_dirty)
        self.access_mode = Value(settings.get('access_mode', 'read_only'), self.mark_dirty)
        self.key = Value(self.saved_key, self.schedule_key_save)
        self.auto_start_status = Value('登入自啟動：尚未檢查')
        self.access_mode_status = Value('目前尚未連線')
        self.status = Value('尚未連線；請確認設定後啟動連線。')
        self.reader_numbers = {name: Value('', self.mark_dirty) for name in NUMERIC_FIELDS}
        self.reader_lists = {name: TextValue('', self.mark_dirty) for name in LIST_FIELDS}
        self.fill_advanced(normalize_reader_settings(settings.get('reader')))
        self.workspace_rows = [dict(row) for row in settings['roots']]
        self.invalid_root_ids = set()
        self.folder_status = Value('')
        self.refresh_folder_list()
        self.suppress_dirty = False
        self.tray = (tray_factory or Tray)(lambda: window.after(0, self.show),
            lambda: window.after(0, self.popup), self.wake_event.set, icon_style=self.icon_style.get())
        self.refresh_controls()
        self.tick_timer = window.after(100, self.tick)

    def activate(self) -> None:
        if self.startup or self.hidden.get():
            self.hide()
        if self.settings.get('auto_connect') and (self.startup or not self.auto_start.get()):
            self.window.after(0, self.start)
        if self.key_load_error:
            self.dialogs.showerror('金鑰讀取失敗', self.key_load_error)
        self.window.after(0, self.check_background)

    def apply_icon(self, style: str) -> None:
        try:
            self.tray.set_icon(style)
            self.window.emit({'event': 'icon', 'style': style})
        except OSError:
            self.dialogs.showwarning('圖示套用失敗', '設定已載入，但圖示無法完整套用。')

    def refresh_controls(self) -> None:
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

    def refresh_folder_list(self) -> None:
        self.folder_status.set(f'已加入 {len(self.workspace_rows)} 個資料夾（最多 8 個）')

    def edit_folder(self, folder: str, index: int | None = None) -> None:
        if self.tasks.busy or self.quitting:
            return
        previous = self.workspace_rows[index] if index is not None else None
        row = create_workspace_root(folder, self.workspace_rows, self.collect_advanced(), previous=previous)
        if index is None:
            self.workspace_rows.append(row)
        else:
            self.workspace_rows[index] = row
        self.invalid_root_ids.discard(row['id'])
        self.refresh_folder_list()
        self.mark_dirty()

    def remove_folders(self, indexes: list[int]) -> None:
        if self.tasks.busy or self.quitting:
            return
        if any(type(index) is not int or not 0 <= index < len(self.workspace_rows) for index in indexes):
            raise ValueError('資料夾選取無效。')
        for index in sorted(set(indexes), reverse=True):
            self.invalid_root_ids.discard(self.workspace_rows[index]['id'])
            del self.workspace_rows[index]
        self.refresh_folder_list()
        self.mark_dirty()

    def refresh_request_monitor(self) -> None:
        self.request_monitor.poll()

    def toggle_request_monitor(self) -> None:
        self.request_monitor.set_enabled(not self.request_monitor.enabled)
        self.wake_tick()

    def clear_request_monitor(self) -> None:
        self.request_monitor.clear()

    def finish(self) -> None:
        if self.finished:
            return
        self.finished = True
        self.request_monitor.close()
        if self.retry_timer is not None:
            self.window.after_cancel(self.retry_timer)
            self.retry_timer = None
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
            self.tick_timer = None
        self.tasks.close()
        self.tray.close()
        self.window.destroy()
        self.wake_event.close()

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
                self.dialogs.showerror('金鑰保存失敗', '無法加密保存 API 金鑰，請確認本機儲存空間與權限。金鑰仍保留在介面中。', parent=self.window)
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

    def show(self) -> None:
        self.window.deiconify()
        self.window.lift()
        self.wake_tick()

    def popup(self) -> None:
        from mcp_local.tray_windows import TrayDesktopUnavailable
        from mcp_local.access_mode import MODES, MODE_LABELS

        controls = self.control_states()
        saved_mode = self.settings.get('access_mode', 'read_only')
        actions = [
            ('啟動連線', controls['start'], self.start),
            ('停止連線', controls['stop'], self.stop),
            ('', False, None),
            *((MODE_LABELS[mode], controls['switch_mode'] and mode != saved_mode,
               lambda mode=mode: self.switch_access_mode(mode), mode == saved_mode)
              for mode in MODES),
            ('', False, None),
            ('退出程式', not self.quitting, self.quit),
        ]
        try:
            self.tray.popup(actions)
        except TrayDesktopUnavailable as exc:
            self.status.set(str(exc))

    def switch_access_mode(self, mode: str) -> None:
        """系統匣切換沿用設定驗證與清理後重連流程。"""
        from mcp_local.access_mode import normalize_access_mode

        mode = normalize_access_mode(mode)
        if (not self.control_states()['switch_mode']
                or mode == self.settings.get('access_mode', 'read_only')):
            return
        previous_mode, was_dirty = self.access_mode.get(), self.dirty
        self.access_mode.set(mode)
        if not self.save(after=self.start, confirm_mode=False):
            self.access_mode.set(previous_mode)
            self.dirty = was_dirty
            self.refresh_controls()

    def hide(self) -> None:
        self.window.withdraw()
        self.wake_tick()

    def wake_tick(self) -> None:
        if self.finished:
            return
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
        self.tick_timer = self.window.after(0, self.tick)

    def tick_interval_ms(self) -> int:
        if self.tasks.busy or not self.connection.events.empty() or not self.tasks.events.empty():
            return 100
        return 200 if self.request_monitor.enabled else 250

    def mark_dirty(self, *args) -> None:
        if not self.suppress_dirty:
            self.dirty = True
            self.edit_generation += 1
            self.refresh_controls()
            self.wake_tick()

    def control_states(self) -> dict:
        available = not self.quitting and not self.tasks.busy
        stable = available and self.state in ('IDLE', 'RUNNING', 'REPAIR')
        bg = self.background_status
        return {
            'switch_mode': stable and bool(self.workspace_rows) and not self.repair_required
                           and not self.connection.cleanup_failed,
            'save': stable and bool(self.workspace_rows) and (self.dirty or self.key_dirty or self.repair_required),
            'start': stable and bool(self.workspace_rows) and not self.running and not self.repair_required
                     and not self.connection.cleanup_failed
                     and not (bg and bg.running),
            'apply': stable and bool(self.workspace_rows) and self.running and not self.repair_required,
            'stop': available and self.state in ('IDLE', 'STARTING', 'RUNNING', 'REPAIR')
                    and (self.running or self.retry_timer is not None
                         or bool(bg and bg.running and bg.task_valid and not bg.error_code)),
            'background_check': stable,
        }

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
                self.dialogs.showerror('停止連線失敗', '無法停止連線；請更新狀態後重試。', parent=self.window)
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
            self.icon_style.set(value.get('icon_style', 'classic'))
            self.access_mode.set(value.get('access_mode', 'read_only'))
            self.apply_icon(self.icon_style.get())
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

    def save(self, after=None, retry_on_conflict=True, *, confirm_mode=True) -> bool:
        if self.tasks.busy or self.quitting or self.state not in ('IDLE', 'RUNNING', 'REPAIR'):
            return False
        try:
            if not self.workspace_rows:
                raise ValueError('至少需要加入一個允許讀取的資料夾。')
            settings = {'root': self.workspace_rows[0]['path'], 'tunnel': self.tunnel.get().strip()}
            settings['recent'] = list(self.settings.get('recent', []))
            settings['start_hidden'] = self.hidden.get()
            settings['auto_start'] = self.auto_start.get()
            settings['auto_connect'] = self.auto_start.get()
            settings['icon_style'] = self.icon_style.get()
            from mcp_local.access_mode import normalize_access_mode
            settings['access_mode'] = normalize_access_mode(self.access_mode.get())
            mode_changed = settings['access_mode'] != self.settings.get('access_mode', 'read_only')
            if confirm_mode and mode_changed and settings['access_mode'] in ('developer_control', 'host_control'):
                from mcp_local.access_mode import MODE_LABELS
                description = ('允許以目前使用者權限讀寫主機檔案與執行受信任命令，可跨原共享 root。\n'
                               '命令不是沙箱，也可讀寫設定與使用者憑證；檔案 API 防護不會限制命令。\n'
                               '不自動提升 Administrator／SYSTEM。'
                               if settings['access_mode'] == 'host_control' else
                               '允許 VM 直接修改下列授權 root：\n'
                               + '\n'.join(row['path'] for row in self.workspace_rows)
                               + '\n\n自動偵測安全安裝目錄中的開發工具鏈，僅唯讀映射；不需手動設定。'
                               + '\n沒有額外工具鏈仍可使用 VM 內建 PowerShell；VM 不可用時拒絕命令。'
                               + '\nroot 為完整映射，包含其中的 .git、隱藏及檔案 API 排除項目。'
                               + '\nVM 不繼承主機憑證，網路及剪貼簿關閉；禁止映射向外硬連結與重新解析點。')
                if not self.dialogs.askyesno('啟用' + MODE_LABELS[settings['access_mode']],
                        description + '\n\n模式會保存。切換會停止舊連線及 sessions 後重新連線。', parent=self.window):
                    return False
            if settings['auto_start'] and (not self.key_store or not self.key.get().strip()):
                raise ValueError('自動啟動需要已保存的 DPAPI 金鑰。')
            settings['reader'] = self.collect_advanced()
            settings['default_root'] = self.workspace_rows[0]['id']
            settings['roots'] = [dict(row) for row in self.workspace_rows]
            if self.settings.get('settings_version') not in range(1, SETTINGS_VERSION + 1):
                if not self.dialogs.askyesno('首次共用設定遷移',
                        '舊 PowerShell 分享範圍為 D:\\codee；GUI 使用獨立設定。\n'
                        f'確認兩個入口今後都使用：\n{settings["root"]}\n{settings["tunnel"]}\n'
                        '確認後會備份並保存一般設定；加密金鑰保留。', parent=self.window):
                    return False
            settings['settings_version'] = SETTINGS_VERSION
            previous = self.settings.copy()
            key = self.key.get().strip()
            saved_key = self.saved_key
            revision = self.settings_revision
            intent = self.intent
            generation = self.edit_generation
            was_dirty = self.dirty
            save_backend = save_settings
            key_state = {'saved': False}
            settings_commit = {'revision': None}
            connection_state = {'external_stopped': False}
            owned_running = self.running
            def work():
                normalized = normalize_connection(settings)
                if mode_changed and not owned_running:
                    background = get_background_status()
                    if background.error_code:
                        raise ValueError('MODE_SWITCH_UNVERIFIED：無法確認背景連線，模式未儲存。')
                    if background.running:
                        if not background.task_valid:
                            raise ValueError('MODE_SWITCH_UNVERIFIED：背景工作不符，請先停止原連線。')
                        stop_background()
                        connection_state['external_stopped'] = True
                if self.key_store and key and key != saved_key:
                    self.key_store.save(key)
                    key_state['saved'] = True
                def save(value):
                    if revision is None:
                        saved_revision = save_backend(value)
                    else:
                        saved_revision = save_backend(value, expected_revision=revision)
                    if isinstance(saved_revision, str) and re.fullmatch(r'[0-9a-f]{64}', saved_revision):
                        settings_commit['revision'] = saved_revision
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
                        reload = (not local_changes and retry_on_conflict) or (local_changes and self.dialogs.askyesno(
                            '設定已被其他操作更新', '磁碟設定已變更，本次儲存已取消。是否捨棄未儲存的一般設定並重新載入？API 金鑰輸入會保留。', parent=self.window))
                        if reload:
                            try:
                                self.reload_settings()
                                if not local_changes and intent == self.intent and not self.repair_required:
                                    self.save(after=after, retry_on_conflict=False, confirm_mode=confirm_mode)
                            except Exception:
                                self.dialogs.showerror('無法重新載入', '設定無法載入；目前編輯內容已保留。', parent=self.window)
                            return
                        if local_changes:
                            return
                    self.dialogs.showerror('無法儲存設定', str(error), parent=self.window)
                    return
                self.settings = value
                self.apply_icon(value['icon_style'])
                self.settings_revision = settings_commit['revision'] or hashlib.sha256(
                    json.dumps(value, ensure_ascii=False, indent=2).encode('utf-8')).hexdigest()
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
                if mode_changed and self.running:
                    self.restart_pending = (intent == self.intent and generation == self.edit_generation and not self.key_dirty)
                    self.connection.cancel.set()
                    self.request_monitor.disconnect()
                    self.state = 'RESTARTING'
                    self.status.set('權限模式已保存；正在停止舊連線及 sessions，完成後重連。')
                    self.refresh_controls()
                elif connection_state['external_stopped'] and intent == self.intent and generation == self.edit_generation:
                    self.launch()
                elif after and intent == self.intent and generation == self.edit_generation and not self.key_dirty:
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
            self.dialogs.showerror('無法儲存設定', str(exc), parent=self.window)
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
            self.dialogs.showinfo('需要金鑰', '請輸入通道 API 金鑰。', parent=self.window)
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
                self.dialogs.showerror('無法啟動', '無法載入最新設定。', parent=self.window)
                return
        self.launch()

    def launch(self) -> None:
        if self.quitting or self.running or self.repair_required:
            return
        if self.connection.cleanup_failed:
            self.status.set('舊連線停止未確認；請檢查並停止殘留程序後重新開啟 APP。')
            return
        if self.connection.thread and self.connection.thread.is_alive():
            self.window.after(50, self.launch)
            return
        # 每次連線使用獨立事件佇列，舊事件不會污染新連線。
        self.connection = Connection(notify=self.wake_event.set, monitor=self.request_monitor)
        try:
            self.connection.start(self.settings.copy(), self.key.get().strip())
            from mcp_local.access_mode import MODE_LABELS
            self.active_access_mode = self.settings.get('access_mode', 'read_only')
            self.access_mode_status.set('本次連線啟動模式：' + MODE_LABELS[self.active_access_mode])
            self.running = True
            self.state = 'STARTING'
            self.refresh_controls()
            self.refresh_request_monitor()
        except Exception:
            self.request_monitor.disconnect()
            self.running = False
            self.state = 'IDLE'
            self.status.set('連線啟動失敗。')
            self.refresh_controls()
            self.refresh_request_monitor()

    def restart(self) -> None:
        if not self.control_states()['apply']:
            return
        def apply():
            self.restart_pending = True
            self.connection.cancel.set()
            self.request_monitor.disconnect()
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
        self.request_monitor.disconnect()
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
        self.refresh_request_monitor()
        if self.quitting and not self.running and not self.tasks.busy:
            self.finish()
            return
        self.tray.pump()
        while not self.connection.events.empty():
            kind, text = self.connection.events.get_nowait()
            if kind == 'error' and self.retry_enabled and self.connection.error_kind in self.retry_policy.retryable:
                self.status.set('連線暫時失敗，將自動重試。')
                continue
            if kind == 'error':
                self.show()
                self.dialogs.showerror('連線未完成', text, parent=self.window)
            elif kind == 'done':
                if self.connection.thread and self.connection.thread.is_alive():
                    self.connection.events.put((kind, text))
                    break
                self.request_monitor.disconnect()
                self.refresh_request_monitor()
                self.running = False
                if self.connection.cleanup_failed:
                    self.restart_pending = False
                    self.retry_enabled = False
                    if self.retry_timer is not None:
                        self.window.after_cancel(self.retry_timer)
                        self.retry_timer = None
                    self.access_mode_status.set('舊連線停止未確認；尚未套用新模式')
                    self.status.set('舊連線清理失敗；請檢查並停止殘留程序後重新開啟 APP。')
                    self.state = 'IDLE'
                    if self.quitting:
                        self.finish()
                        return
                    break
                self.active_access_mode = None
                self.access_mode_status.set('目前尚未連線')
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
        if self.tick_timer is not None:
            self.window.after_cancel(self.tick_timer)
        self.tick_timer = self.window.after(self.tick_interval_ms(), self.tick)
