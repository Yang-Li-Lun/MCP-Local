"""隔離桌面整合夾具；只允許新建的專用目錄，排程與電源均為替身。"""
from __future__ import annotations
from pathlib import Path
import os
import sys
import subprocess
import uuid
from unittest.mock import Mock, patch


def configure_fixture(directory: Path, stack, *, native_tray: bool = False):
    directory = directory.resolve()
    if not directory.is_dir() or any(directory.iterdir()):
        raise ValueError('桌面 fixture 必須使用已建立且空白的專用目錄。')
    production = (Path(os.environ['LOCALAPPDATA']) / 'MCP-Local').resolve()
    if directory == production or directory.is_relative_to(production) or production.is_relative_to(directory):
        raise ValueError('fixture 不可使用正式設定目錄。')
    def guard(event, arguments):
        if event == 'open' and isinstance(arguments[0], (str, bytes, os.PathLike)):
            if Path(os.fsdecode(arguments[0])).absolute().is_relative_to(production):
                raise RuntimeError('FIXTURE_PRODUCTION_STATE_BLOCKED')
        if event == 'mcp.power.mutate' or event in ('socket.connect', 'socket.bind'):
            raise RuntimeError('FIXTURE_EXTERNAL_EFFECT_BLOCKED')
        if event == 'subprocess.Popen':
            # Windows Popen commonly reports executable=None and a quoted command line.
            allowed = {str(path) for name in (sys.executable, sys._base_executable)
                       for path in (Path(name).absolute(), Path(name).resolve())}
            executable, command = arguments[:2]
            permitted = (str(Path(executable).resolve()) in allowed if executable else
                         isinstance(command, str) and any(command.startswith(subprocess.list2cmdline([path]) + ' ') for path in allowed))
            if not permitted:
                raise RuntimeError('FIXTURE_NON_PYTHON_PROCESS_BLOCKED')
    sys.addaudithook(guard)
    import json
    (directory / 'processes.json').write_text(json.dumps({'backend': os.getpid(), 'frontend': os.getppid()}), encoding='utf-8')
    from mcp_local import desktop_controller as controller
    from mcp_local.connection_settings import normalize_connection, save_settings, load_for_edit
    from mcp_local.key_store import KeyStore
    from mcp_local.autostart_windows import BackgroundStatus
    shared = directory / 'shared'
    shared.mkdir()
    settings = normalize_connection({'settings_version': 8, 'roots': [{'id': 'fixture', 'path': str(shared), 'excluded_names': ['private']}],
        'default_root': 'fixture', 'tunnel': 'tunnel_fixture', 'power': {'mode': 'off'}})
    if native_tray:
        settings['icon_style'] = 'folder_link'
    target = directory / 'settings.json'
    save_settings(settings, target)
    store = KeyStore(directory / 'fixture-key.dpapi')
    store.save('synthetic-fixture-key')
    tray = Mock(return_value=Mock())
    if native_tray:
        tray = native_fixture_tray(directory)
    stack.enter_context(patch.object(controller, 'load_for_edit', lambda: load_for_edit(target)))
    stack.enter_context(patch.object(controller, 'save_settings', lambda value, **kw: save_settings(value, target, **kw)))
    stack.enter_context(patch.object(controller, 'get_background_status', return_value=BackgroundStatus(False, False, False)))
    stack.enter_context(patch.object(controller, 'stop_background', side_effect=RuntimeError('FIXTURE_BACKGROUND_STOP_BLOCKED')))
    stack.enter_context(patch.object(controller, 'apply_settings_transaction', side_effect=lambda value, old, save: save(value)))
    stack.enter_context(patch('mcp_local.connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_WPF_Fixture_' + uuid.uuid4().hex))
    stack.enter_context(patch('mcp_local.connection_runtime.existing_tunnel', return_value=False))
    stack.enter_context(patch('mcp_local.developer_vm_client.GuardOwner.__init__', side_effect=RuntimeError('FIXTURE_VM_BLOCKED')))
    script = ('import os,time; from mcp_local.request_monitor import MemoryChannel,ENV_NAME; '
              'c=MemoryChannel(os.environ[ENV_NAME]); e=c.epoch(); '
              'c.publish(e,dict(id="fixture-call",time="2026-11-01T12:00:00+08:00",tool="read_file",status="success",'
              'elapsed_ms=1.25,request={"params":{"arguments":{"path":"synthetic.txt"}}},'
              'response={"text":"合成整合資料"},response_notes=[{"path":"/text","reason":"fixture note","retained_chars":6}])); '
              'time.sleep(60)')
    stack.enter_context(patch('mcp_local.connection_runtime.build_commands', return_value=[
        [sys.executable, '-B', '-c', 'pass'], [sys.executable, '-B', '-c', 'pass'], [sys.executable, '-B', '-c', script]]))
    return settings, store, tray


def native_fixture_tray(directory: Path):
    """Real, separately identified icon; only bounded synthetic lifecycle evidence is saved."""
    import json
    import time
    from mcp_local.tray_windows import Tray, shell, c

    label = 'WPF 隔離測試 ' + directory.name[-8:]

    class FixtureTray(Tray):
        def __init__(self, show, menu, notify=None, **kwargs):
            self.observations = []
            self.written = -1
            super().__init__(show, menu, notify, **kwargs)
            self.record('created')
            self.update('尚未連線')
            self.flush_evidence()

        def record(self, event):
            if len(self.observations) < 64:
                self.observations.append({'event': event, 'monotonic': time.monotonic()})

        def _message(self, hwnd, message, wp, lp):
            if message == 0x8001 and lp in (0x202, 0x203, 0x205):
                self.record({0x202: 'native_left_release', 0x203: 'native_double_click', 0x205: 'native_right_release'}[lp])
            return super()._message(hwnd, message, wp, lp)

        def update(self, text):
            self.data.tip = (label + '：' + text)[:127]
            shell.Shell_NotifyIconW(1, c.byref(self.data))

        def pump(self):
            super().pump()
            self.flush_evidence()

        def flush_evidence(self):
            if self.written != len(self.observations):
                (directory / 'native-tray-events.json').write_text(json.dumps({
                    'label': label, 'backend_pid': os.getpid(), 'tray_window': self.window,
                    'synthetic_only': True, 'events': self.observations}, ensure_ascii=False, indent=2), encoding='utf-8')
                self.written = len(self.observations)

        def close(self):
            super().close()
            self.record('closed')
            self.flush_evidence()

    return FixtureTray


def install_monitor_load(controller):
    """Existing resize baseline: 200 records, 26.52M response chars, four updates/100ms."""
    import json
    monitor = controller.request_monitor
    monitor.begin_connection()
    for index in range(200):
        row = {'id': str(index), 'time': f'2026-11-01T12:{index // 60:02d}:{index % 60:02d}+08:00',
               'tool': 'list_directory' if index % 7 == 0 else 'read_file',
               'status': 'running' if index < 4 else 'success', 'elapsed_ms': float(index),
               'request': {'jsonrpc': '2.0', 'id': index, 'method': 'tools/call', 'params': {
                   'name': 'read_file', 'arguments': {'path': f'資料/{index}.txt', 'line_count': 3}}},
               'response': {'text': 'x' * (1048000 if index == 199 else 128000)}}
        row['_revision'] = index + 1
        row['_bytes'] = len(json.dumps(row, ensure_ascii=False).encode('utf-8'))
        monitor.records[row['id']] = row
        monitor.used += row['_bytes']
    monitor.revision = 200
    def advance():
        if controller.finished:
            return
        if monitor.enabled:
            for index in range(4):
                if row := monitor.records.get(str(index)):
                    monitor.revision += 1
                    row['elapsed_ms'] += 100
                    row['_revision'] = monitor.revision
        controller.window.after(100, advance)
    controller.window.after(100, advance)
