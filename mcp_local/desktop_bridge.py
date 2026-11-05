"""WPF 專用子程序管線；單一控制器、有限命令，不提供網路端點。"""
from __future__ import annotations

import argparse
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor
import copy
import ctypes
import json
import os
from pathlib import Path
import queue
import sys
import threading
import time
import uuid

PROTOCOL = 1
MAX_INPUT = 1024 * 1024


class Pipe:
    def __init__(self, input_stream, output_stream):
        self.input, self.output = input_stream, output_stream
        self.commands: queue.Queue = queue.Queue(maxsize=32)
        self.answers: queue.Queue = queue.Queue(maxsize=1)
        self.closed = threading.Event()
        self.lock = threading.Lock()
        self.pending_dialog = None
        self.outgoing: queue.Queue = queue.Queue(maxsize=4)
        self.notify = lambda: None
        threading.Thread(target=self.write, name='wpf-event-writer', daemon=True).start()

    def write(self) -> None:
        while not self.closed.is_set():
            try:
                data = self.outgoing.get(timeout=.1)
            except queue.Empty:
                continue
            try:
                offset = 0
                while offset < len(data):
                    offset += os.write(self.output.fileno(), data[offset:])
            except (OSError, ValueError):
                self.closed.set()
                self.notify()
            finally:
                self.outgoing.task_done()

    def emit(self, value: dict) -> None:
        if self.closed.is_set():
            return
        data = (json.dumps(value, ensure_ascii=True, separators=(',', ':')) + '\n').encode('utf-8')
        try:
            self.outgoing.put_nowait(data)
        except queue.Full:
            # A stalled client cannot block connection stop.
            self.closed.set()
            self.notify()

    def read(self) -> None:
        try:
            buffered = b''
            while not self.closed.is_set():
                while b'\n' not in buffered and len(buffered) <= MAX_INPUT:
                    # Raw descriptor I/O avoids holding Python's buffered-stdin lock
                    # while an idle client remains connected during interpreter exit.
                    chunk = os.read(self.input.fileno(), 8192)
                    if not chunk:
                        return
                    buffered += chunk
                line, separator, buffered = buffered.partition(b'\n')
                if len(line) > MAX_INPUT or not separator:
                    break
                value = json.loads(line)
                if not isinstance(value, dict) or type(value.get('id')) is not int or not isinstance(value.get('command'), str):
                    break
                if value['command'] == 'dialog_answer':
                    if value.get('token') == self.pending_dialog and type(value.get('accepted')) is bool:
                        try:
                            self.answers.put_nowait(value['accepted'])
                        except queue.Full:
                            pass
                else:
                    try:
                        self.commands.put_nowait(value)
                        self.notify()
                    except queue.Full:
                        break
        except (OSError, UnicodeError, ValueError):
            pass
        finally:
            self.closed.set()
            self.notify()


class PipeDialogs:
    def __init__(self, pipe: Pipe):
        self.pipe = pipe

    def askyesno(self, title: str, text: str, **kwargs) -> bool:
        if self.pipe.closed.is_set():
            return False
        token = uuid.uuid4().hex
        self.pipe.pending_dialog = token
        self.pipe.emit({'event': 'dialog', 'kind': 'confirm', 'token': token, 'title': title, 'text': text})
        try:
            while not self.pipe.closed.is_set():
                try:
                    return self.pipe.answers.get(timeout=.1)
                except queue.Empty:
                    pass
            return False
        finally:
            self.pipe.pending_dialog = None
            while not self.pipe.answers.empty():
                self.pipe.answers.get_nowait()

    def showerror(self, title: str, text: str, **kwargs) -> None:
        self.pipe.emit({'event': 'dialog', 'kind': 'error', 'title': title, 'text': text})

    showinfo = showerror
    showwarning = showerror


class DesktopApi:
    def __init__(self, controller):
        self.controller = controller
        self._monitor_key = None
        self._monitor_version = 0
        self._monitor_rows = {}
        self._monitor_snapshot = None

    def form(self) -> dict:
        a = self.controller
        return {'tunnel': a.tunnel.get(), 'start_hidden': a.hidden.get(), 'auto_start': a.auto_start.get(),
                'icon_style': a.icon_style.get(), 'access_mode': a.access_mode.get(),
                'numbers': {k: v.get() for k, v in a.reader_numbers.items()},
                'lists': {k: v.get() for k, v in a.reader_lists.items()},
                'roots': a.workspace_rows, 'invalid_root_ids': sorted(a.invalid_root_ids)}

    def state(self) -> dict:
        a = self.controller
        return {'state': a.state, 'running': a.running, 'busy': a.tasks.busy, 'quitting': a.quitting,
                'finished': a.finished, 'dirty': a.dirty or a.key_dirty, 'controls': a.control_states(),
                'status': a.status.get(), 'background_status': a.auto_start_status.get(),
                'access_status': a.access_mode_status.get(), 'key_available': bool(a.saved_key),
                'key_load_error': a.key_load_error, 'generation': a.edit_generation,
                'visible': a.window.visible, 'form': self.form()}

    def set_form(self, value: dict) -> None:
        a = self.controller
        if not isinstance(value, dict) or set(value) - {'tunnel', 'start_hidden', 'auto_start', 'icon_style', 'access_mode', 'numbers', 'lists', 'key'}:
            raise ValueError('無效編輯欄位。')
        for name in ('start_hidden', 'auto_start'):
            if name in value and type(value[name]) is not bool:
                raise ValueError('旗標須為布林值。')
        for name in ('tunnel', 'icon_style', 'access_mode', 'key'):
            if name in value and (not isinstance(value[name], str) or len(value[name]) > (16384 if name == 'key' else 4096)):
                raise ValueError('欄位格式或長度無效。')
        for group in ('numbers', 'lists'):
            fields = getattr(a, 'reader_' + group)
            proposed = value.get(group, {})
            if not isinstance(proposed, dict) or set(proposed) - set(fields) or any(not isinstance(v, str) or len(v) > 32768 for v in proposed.values()):
                raise ValueError('進階讀取欄位無效。')
        # Validate the complete envelope before changing any draft field.
        for name, target in (('tunnel', a.tunnel), ('start_hidden', a.hidden), ('auto_start', a.auto_start),
                             ('icon_style', a.icon_style), ('access_mode', a.access_mode)):
            if name in value:
                target.set(value[name])
        for group in ('numbers', 'lists'):
            for name, item in value.get(group, {}).items():
                getattr(a, 'reader_' + group)[name].set(item)
        if 'key' in value:
            # An empty PasswordBox never deletes a persisted credential.
            a.key.set(value['key'].strip() or a.saved_key)

    def monitor(self, since=None) -> dict:
        from mcp_local.request_monitor_presenter import argument_summary
        a = self.controller
        m = a.request_monitor
        status = {'enabled': m.enabled, 'connected': m.connected, 'attached': m.attached, 'failed': m.failed,
                  'lost_events': m.lost_events, 'evicted_records': m.evicted_records,
                  'dropped_total': m.dropped, 'captured_bytes': m.used}
        key = (tuple((row['id'], row['_revision']) for row in m.records.values()), tuple(status.values()))
        if key != self._monitor_key:
            rows = {}
            for row in m.records.values():
                cached = self._monitor_rows.get(row['id'])
                if cached is None or cached['_revision'] != row['_revision']:
                    cached = {**{k: v for k, v in row.items() if k in
                                 ('id', 'time', 'tool', 'status', 'elapsed_ms', '_revision')},
                              'summary': argument_summary(row),
                              'partial': bool(row.get('request_notes') or row.get('response_notes'))}
                rows[row['id']] = cached
            self._monitor_rows = rows
            self._monitor_key = key
            self._monitor_version += 1
            self._monitor_snapshot = {**status, 'records': list(rows.values()), 'version': self._monitor_version}
        if type(since) is int and since == self._monitor_version:
            return {'unchanged': True, 'version': self._monitor_version}
        return self._monitor_snapshot

    def dispatch(self, command: str, args: dict) -> dict:
        a = self.controller
        if a.quitting and command not in ('state', 'quit', 'monitor'):
            raise ValueError('程式正在結束。')
        if command == 'state':
            return self.state()
        if command == 'hello':
            from mcp_local.reader_settings import NUMERIC_FIELDS, normalize_reader_settings
            from mcp_local.access_mode import MODE_LABELS
            return {'protocol': PROTOCOL, 'state': self.state(), 'modes': MODE_LABELS,
                    'numbers': NUMERIC_FIELDS, 'defaults': normalize_reader_settings()}
        if command == 'diagnostics':
            from mcp_local.desktop_identity import diagnostics
            return diagnostics()
        if command.startswith('security_'):
            return self.security_dispatch(command, args)
        if command == 'edit':
            self.set_form(args['form'])
        elif command in ('save', 'start', 'restart'):
            if 'form' in args:
                self.set_form(args['form'])
            getattr(a, command)()
        elif command in ('stop', 'quit', 'check_background', 'toggle_request_monitor', 'clear_request_monitor'):
            getattr(a, command)()
        elif command == 'visibility':
            a.window.visible = args.get('visible') is True
            a.wake_tick()
        elif command == 'folder':
            index = args.get('index')
            if index is not None and (type(index) is not int or not 0 <= index < len(a.workspace_rows)):
                raise ValueError('資料夾索引無效。')
            if not isinstance(args.get('path'), str) or len(args['path']) > 4096:
                raise ValueError('資料夾路徑無效。')
            a.edit_folder(args['path'], index)
        elif command == 'remove_folders':
            a.remove_folders(args['indexes'])
        elif command == 'reader_defaults':
            from mcp_local.reader_settings import normalize_reader_settings
            a.fill_advanced(normalize_reader_settings())
        elif command == 'reader_maximums':
            a.set_maximums()
        elif command == 'monitor':
            return self.monitor(args.get('since'))
        elif command in ('detail', 'export'):
            from mcp_local.request_monitor_presenter import record_document
            row = a.request_monitor.records.get(args.get('record_id'))
            return {'document': record_document(row, a.request_monitor) if row else None}
        else:
            raise ValueError('不支援的桌面操作。')
        return self.state()

    def security_dispatch(self, command: str, args: dict) -> dict:
        center = self.controller.security
        if center is None:
            raise ValueError('SECURITY_UNAVAILABLE：安全中心尚未建立。')
        if command == 'security_state':
            return center.snapshot()
        if command == 'security_defaults':
            from mcp_local.policy import defaults
            return {'policy': defaults(), 'draft_only': True}
        if command == 'security_preview':
            return center.preview(args['policy'], args.get('sample', 'credentials'))
        if command == 'security_detect':
            from mcp_local.policy import normalize
            from mcp_local.sensitive_detector import SensitiveDetector, SYNTHETIC
            policy = normalize(args['policy'])
            masked, findings = SensitiveDetector(policy).inspect(SYNTHETIC)
            return dict(enabled=policy['detector_enabled'], synthetic_only=True, findings=findings, preview=masked)
        if command == 'security_events':
            return center.audit.query(args.get('kind', ''), args.get('risk', ''), args.get('offset', 0))
        if command == 'security_detail':
            rows = center.audit.read()
            return {'row': next((row for row in rows if row['event']['id'] == args.get('id')), None)}
        if command == 'security_export':
            return center.audit.export()
        confirmations = {
            'security_save': '儲存此安全政策並套用至本次連線？所有現有主機限時授權會撤銷。啟用防護會遮罩或阻擋回傳，執行中輸出暫存至結束後才可檢查。',
            'security_grant': '由您在本機授予主機命令限時執行權限？這不是憑證沙箱；受信任命令具有目前 Windows 使用者權限。',
            'security_revoke': '立即撤銷主機命令授權？既有 Session 依已儲存政策終止或執行至原期限；不再接受互動輸入。',
            'security_recover': '重新驗證政策、事件及本機通訊？授權保持撤銷，必須另行本機核准。損毀或遺失原檔不會自動覆寫。',
            'security_clear': '清除目前已檢視的安全事件？請先匯出必要證據；原始請求監控不受此操作影響。',
        }
        if command not in confirmations:
            raise ValueError('SECURITY_COMMAND：不支援的安全操作。')
        if not self.controller.dialogs.askyesno('安全中心｜本機確認', confirmations[command]):
            raise ValueError('SECURITY_CANCELLED：本機未確認，未執行操作。')
        if command == 'security_save':
            return center.save(args['policy'], args['expected_revision'])
        if command == 'security_grant':
            return center.grant(args['minutes'])
        if command == 'security_revoke':
            return center.revoke()
        if command == 'security_recover':
            return center.recover()
        with center.lock:
            center.audit.clear(args['expected_revision'], args['confirmation'])
        return center.audit.query()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--startup', action='store_true')
    parser.add_argument('--fixture', type=Path)
    parser.add_argument('--fixture-load', action='store_true')
    parser.add_argument('--fixture-native-tray', action='store_true')
    args = parser.parse_args()
    if (args.fixture_load or args.fixture_native_tray) and not args.fixture:
        parser.error('fixture options require --fixture')
    pipe = Pipe(sys.stdin.buffer, sys.stdout.buffer)
    threading.Thread(target=pipe.read, name='wpf-command-reader', daemon=True).start()
    from contextlib import ExitStack
    mutex = None
    controller = None
    with ExitStack() as stack:
        try:
            if args.fixture:
                from mcp_local.desktop_fixture import configure_fixture
                settings, store, tray = configure_fixture(args.fixture, stack, native_tray=args.fixture_native_tray)
                from mcp_local.connection_settings import load_for_edit
                editable = load_for_edit(args.fixture / 'settings.json')
            else:
                from mcp_local.connection_settings import load_for_edit, retire_legacy_power_setting, CONFIG_DIR
                from mcp_local.key_store import KeyStore
                from mcp_local.tray_windows import kernel
                mutex = kernel.CreateMutexW(None, False, 'Local\\MCP_Local_Settings_UI')
                if not mutex:
                    pipe.emit({'event': 'fatal', 'text': '無法建立程式鎖定。'})
                    return 1
                if ctypes.get_last_error() == 183:
                    if not args.startup:
                        pipe.emit({'event': 'dialog', 'kind': 'info', 'title': '程式已開啟', 'text': '請在系統匣點選 MCP-Local。'})
                    pipe.emit({'event': 'closed'})
                    return 0
                editable = load_for_edit()
                settings = editable.settings
                if args.startup and not settings.get('auto_start'):
                    pipe.emit({'event': 'closed'})
                    return 0
                from mcp_local.legacy_power_recovery import recover_legacy_power
                recover_legacy_power(CONFIG_DIR)
                if retire_legacy_power_setting():
                    editable = load_for_edit()
                    settings = editable.settings
                store, tray = KeyStore(CONFIG_DIR / 'api-key.dpapi'), None
            from mcp_local.desktop_ports import Scheduler
            from mcp_local.desktop_controller import DesktopController
            controller = DesktopController(Scheduler(pipe.emit), settings, store, startup=args.startup,
                dialogs=PipeDialogs(pipe), tray_factory=tray)
            from mcp_local.security_center import SecurityCenter
            from mcp_local.connection_settings import CONFIG_DIR
            controller.security = SecurityCenter(args.fixture if args.fixture else CONFIG_DIR)
            pipe.notify = controller.wake_event.set
            if editable:
                controller.settings_revision = editable.revision if editable.revision is not None else ''
                controller.repair_required = editable.state == 'REPAIR_REQUIRED'
                if controller.repair_required:
                    controller.state = 'REPAIR'
                    controller.invalid_root_ids = {row['root_id'] for row in editable.errors}
                    controller.status.set('修復模式：請變更或移除失效資料夾。')
            controller.activate()
            if args.fixture_load:
                from mcp_local.desktop_fixture import install_monitor_load
                install_monitor_load(controller)
            if controller.repair_required:
                controller.show()
            api = DesktopApi(controller)
            fixture_counts = {}
            pool = stack.enter_context(ThreadPoolExecutor(max_workers=1, thread_name_prefix='monitor-search'))
            search_pending = None
            disconnected_at = None
            def search(request, snapshot):
                from mcp_local.request_monitor_presenter import matches
                try:
                    args = request.get('args', {})
                    query = args.get('query', '').casefold()
                    if not isinstance(query, str) or len(query) > 256:
                        raise ValueError('搜尋字串無效。')
                    ids = [row['id'] for row in snapshot if matches(row, query, args.get('tool', ''), args.get('status', ''))]
                    pipe.emit({'id': request['id'], 'result': {'ids': ids}})
                except Exception:
                    pipe.emit({'id': request['id'], 'error': '搜尋未完成。'})
            while not controller.finished:
                controller.tray.pump()
                if pipe.closed.is_set() and not controller.quitting:
                    if disconnected_at is None:
                        disconnected_at = time.monotonic()
                    controller.quit()
                if disconnected_at is not None and time.monotonic() - disconnected_at > 15:
                    # The finally path stops only our Job; independent recovery guards remain alive.
                    return 1
                controller.window.update()
                if controller.finished:
                    break
                if controller.wake_event.is_set():
                    controller.wake_event.clear()
                    controller.wake_tick()
                try:
                    request = pipe.commands.get_nowait()
                except queue.Empty:
                    delay = max(0, controller.window.timers[0][0] - time.monotonic()) if controller.window.timers else 1
                    controller.wake_event.wait(min(delay, 1))
                    continue
                try:
                    if request['command'] == 'monitor_search':
                        if search_pending is not None and not search_pending.done():
                            pipe.emit({'id': request['id'], 'error': '搜尋忙碌中，請重試。'})
                        else:
                            snapshot = [dict(row) for row in controller.request_monitor.records.values()]
                            search_pending = pool.submit(search, request, snapshot)
                    else:
                        command = request['command']
                        if args.fixture and command == 'fixture_counts':
                            result = dict(fixture_counts)
                        elif args.fixture and command == 'fixture_security_peer':
                            from mcp_local.security_center import SecurityRuntime
                            from mcp_local.local_channel import ENV_NAME
                            peer = SecurityRuntime(args.fixture, controller.security.environment()[ENV_NAME])
                            peer.mode = 'host_control'  # Synthetic peer; no host process is created.
                            peer.start()
                            stack.callback(peer.close)
                            result = {'synthetic_only': True}
                        elif args.fixture and command == 'fixture_emit':
                            m = controller.request_monitor
                            number = request.get('args', {}).get('number', 0)
                            m.channel.publish(m.channel.epoch(), dict(id=f'live-{number}',
                                time='2026-11-05T12:00:00+08:00', tool='read_file', status='success',
                                request={'params': {'arguments': {'path': 'synthetic'}}}, response={'text': '合成更新'}))
                            result = api.state()
                        else:
                            if args.fixture:
                                fixture_counts[command] = fixture_counts.get(command, 0) + 1
                            result = api.dispatch(command, request.get('args', {}))
                        delay = request.get('args', {}).get('_fixture_reply_delay_ms', 0) if args.fixture else 0
                        if type(delay) is int and 0 < delay <= 2000:
                            # Fault injection only after the isolated fixture has been installed;
                            # background collection and transactions keep running during a late reply.
                            controller.window.after(delay, lambda reply={'id': request['id'], 'result': result}: pipe.emit(reply))
                        else:
                            pipe.emit({'id': request['id'], 'result': result})
                except (ValueError, KeyError, IndexError, TypeError) as error:
                    pipe.emit({'id': request['id'], 'error': str(error)})
                except Exception:
                    pipe.emit({'id': request['id'], 'error': '操作未完成，請檢查設定與狀態。'})
            pipe.emit({'event': 'closed'})
            deadline = time.monotonic() + 1
            while pipe.outgoing.unfinished_tasks and not pipe.closed.is_set() and time.monotonic() < deadline:
                time.sleep(.01)
            return 0
        except Exception as error:
            pipe.emit({'event': 'fatal', 'text': str(error)})
            return 1
        finally:
            deadline = time.monotonic() + 1
            while pipe.outgoing.unfinished_tasks and not pipe.closed.is_set() and time.monotonic() < deadline:
                time.sleep(.01)
            if controller:
                controller.connection.stop()
                if controller.connection.thread:
                    controller.connection.thread.join(timeout=15)
                controller.request_monitor.close()
                if controller.security:
                    controller.security.close()
                if not controller.finished:
                    controller.tray.close()
                controller.wake_event.close()
            if mutex:
                from mcp_local.tray_windows import kernel
                kernel.CloseHandle(mutex)


if __name__ == '__main__':
    raise SystemExit(main())
