"""目前使用者的非提升權限登入工作；參數不含認證。"""
from dataclasses import dataclass
import csv
import io
import subprocess
import tempfile
import time
import xml.etree.ElementTree as ET
from pathlib import Path
from connection_settings import PROJECT

NS = 'http://schemas.microsoft.com/windows/2004/02/mit/task'


def _run(arguments: list[str]) -> subprocess.CompletedProcess:
    return subprocess.run(arguments, capture_output=True, timeout=30,
                          creationflags=subprocess.CREATE_NO_WINDOW)


def current_sid() -> str:
    result = _run(['whoami.exe', '/user', '/fo', 'csv', '/nh'])
    if result.returncode:
        raise ValueError('無法取得目前使用者。')
    rows = list(csv.reader(io.StringIO(result.stdout.decode(errors='replace'))))
    sid = rows[0][1] if rows and len(rows[0]) > 1 else ''
    if not sid.startswith('S-1-') or any(c not in 'S-0123456789' for c in sid):
        raise ValueError('使用者識別碼無效。')
    return sid


def current_account() -> str:
    result = _run(['whoami.exe'])
    if result.returncode:
        raise ValueError('無法取得目前使用者。')
    return result.stdout.decode('mbcs').strip()


def task_name() -> str:
    return 'MCP-Local Auto Connect ' + current_sid()


def task_xml() -> str:
    task = ET.Element('Task', xmlns=NS, version='1.2')
    triggers = ET.SubElement(task, 'Triggers')
    trigger = ET.SubElement(triggers, 'LogonTrigger')
    ET.SubElement(trigger, 'Enabled').text = 'true'
    ET.SubElement(trigger, 'UserId').text = current_sid()
    principals = ET.SubElement(task, 'Principals')
    principal = ET.SubElement(principals, 'Principal', id='Author')
    for name, value in [('UserId', current_sid()), ('LogonType', 'InteractiveToken'), ('RunLevel', 'LeastPrivilege')]:
        ET.SubElement(principal, name).text = value
    settings = ET.SubElement(task, 'Settings')
    for name, value in [('MultipleInstancesPolicy', 'IgnoreNew'), ('DisallowStartIfOnBatteries', 'false'),
                        ('StopIfGoingOnBatteries', 'false'), ('AllowStartOnDemand', 'true'),
                        ('Enabled', 'true'), ('Hidden', 'true'), ('ExecutionTimeLimit', 'PT0S')]:
        ET.SubElement(settings, name).text = value
    restart = ET.SubElement(settings, 'RestartOnFailure')
    ET.SubElement(restart, 'Interval').text = 'PT1M'
    ET.SubElement(restart, 'Count').text = '3'
    action = ET.SubElement(ET.SubElement(task, 'Actions', Context='Author'), 'Exec')
    ET.SubElement(action, 'Command').text = str(PROJECT / '.venv/Scripts/pythonw.exe')
    ET.SubElement(action, 'Arguments').text = subprocess.list2cmdline(['-B', str(PROJECT / 'autostart.py')])
    ET.SubElement(action, 'WorkingDirectory').text = str(PROJECT)
    return ET.tostring(task, encoding='unicode')


def _query():
    result = _run(['schtasks.exe', '/Query', '/TN', task_name(), '/XML'])
    if result.returncode:
        listing = _run(['schtasks.exe', '/Query', '/FO', 'CSV', '/NH'])
        if listing.returncode:
            raise ValueError('TASK_QUERY_FAILED：無法查詢工作排程。')
        names = [row[0].lstrip('\\') for row in csv.reader(io.StringIO(listing.stdout.decode('mbcs'))) if row]
        if task_name().casefold() in [name.casefold() for name in names]:
            raise ValueError('TASK_QUERY_FAILED：同名工作存在但无法讀取。')
        return None
    # schtasks emits XML in the console encoding or UTF-16, depending on host.
    data = result.stdout
    for encoding in ('utf-16', 'utf-8-sig', 'mbcs'):
        try:
            return ET.fromstring(data.decode(encoding))
        except (UnicodeError, ET.ParseError):
            pass
    raise ValueError('自動啟動工作格式無效。')


def is_registered() -> bool:
    return _query() is not None


def validate_registered_task() -> bool:
    actual = _query()
    if actual is None:
        return False
    expected = ET.fromstring(task_xml())
    for section in ('Triggers', 'Principals', 'Actions'):
        nodes = actual.findall(f'{{{NS}}}{section}')
        if len(nodes) != 1:
            return False
        if len(nodes[0]) != 1:
            return False
    def value(path, default=None):
        return actual.findtext('/'.join(f'{{{NS}}}{part}' for part in path.split('/')), default)
    if value('Principals/Principal/UserId') != current_sid():
        return False
    if value('Principals/Principal/LogonType') != 'InteractiveToken':
        return False
    if value('Principals/Principal/RunLevel', 'LeastPrivilege') != 'LeastPrivilege':
        return False
    trigger = actual.find(f'{{{NS}}}Triggers')[0]
    if trigger.tag != f'{{{NS}}}LogonTrigger' or value('Triggers/LogonTrigger/Enabled', 'true') != 'true':
        return False
    if value('Triggers/LogonTrigger/UserId', '').casefold() not in (current_sid().casefold(), current_account().casefold()):
        return False
    action = actual.find(f'{{{NS}}}Actions')[0]
    if action.tag != f'{{{NS}}}Exec' or len(action) != 3:
        return False
    for field in ('Command', 'Arguments', 'WorkingDirectory'):
        path = f'{{{NS}}}Actions/{{{NS}}}Exec/{{{NS}}}{field}'
        if actual.findtext(path) != expected.findtext(path):
            return False
    for field, default in [('MultipleInstancesPolicy', None), ('ExecutionTimeLimit', None),
                           ('Enabled', 'true'), ('Hidden', 'false'),
                           ('DisallowStartIfOnBatteries', 'true'), ('StopIfGoingOnBatteries', 'true')]:
        if value('Settings/' + field, default) != expected.findtext(f'{{{NS}}}Settings/{{{NS}}}{field}'):
            return False
    if value('Settings/RestartOnFailure/Count') != '3' or value('Settings/RestartOnFailure/Interval') != 'PT1M':
        return False
    return True


def register_autostart() -> None:
    if is_registered():
        if validate_registered_task():
            return
        raise ValueError('TASK_MISMATCH：同名工作不相符，已保留。')
    if not (PROJECT / '.venv/Scripts/pythonw.exe').is_file():
        raise ValueError('找不到背景 Python 執行檔。')
    with tempfile.TemporaryDirectory(prefix='mcp-task-') as directory:
        path = Path(directory) / 'task.xml'
        path.write_text(task_xml(), encoding='utf-16')
        result = _run(['schtasks.exe', '/Create', '/TN', task_name(), '/XML', str(path)])
    if result.returncode or not validate_registered_task():
        raise ValueError('自動啟動註冊或驗證失敗；請檢查目前使用者的工作排程權限。')


def unregister_autostart() -> None:
    if not is_registered():
        return
    if not validate_registered_task():
        raise ValueError('工作設定不一致，已保留供檢查。')
    result = _run(['schtasks.exe', '/Delete', '/TN', task_name(), '/F'])
    if result.returncode or is_registered():
        raise ValueError('無法移除自動啟動工作。')


@dataclass(frozen=True)
class BackgroundStatus:
    registered: bool
    task_valid: bool
    running: bool
    error_code: str | None = None


def get_background_status() -> BackgroundStatus:
    try:
        registered = is_registered()
        valid = registered and validate_registered_task()
        return BackgroundStatus(registered, valid, background_running())
    except Exception:
        return BackgroundStatus(False, False, False, 'BACKGROUND_STATUS_FAILED')


def _require_task() -> None:
    if not validate_registered_task():
        raise ValueError('TASK_MISMATCH：登入自啟動工作遺失或設定不一致，已停止控制。')


def _wait_background(expected: bool) -> None:
    deadline = time.monotonic() + 10
    while background_running() != expected:
        if time.monotonic() >= deadline:
            code = 'BACKGROUND_START_FAILED' if expected else 'BACKGROUND_STOP_FAILED'
            raise ValueError(code + '：無法確認背景程序已完成操作。')
        time.sleep(.1)


def start_background() -> None:
    _require_task()
    if background_running():
        return
    if _run(['schtasks.exe', '/Run', '/TN', task_name()]).returncode:
        raise ValueError('BACKGROUND_START_FAILED：背景連線啟動失敗。')
    _wait_background(True)


def stop_background() -> None:
    _require_task()
    if not background_running():
        return
    if _run(['schtasks.exe', '/End', '/TN', task_name()]).returncode:
        raise ValueError('BACKGROUND_STOP_FAILED：背景連線停止失敗。')
    _wait_background(False)


def restart_background() -> None:
    _require_task()
    stop_background()
    start_background()


def control_background(restart: bool = False) -> None:
    """相容舊呼叫端；GUI 使用明確的三個操作。"""
    (restart_background if restart else stop_background)()


def background_running() -> bool:
    import ctypes
    from ctypes import wintypes
    from tray_windows import kernel, signature
    signature(kernel, 'OpenMutexW', wintypes.HANDLE, wintypes.DWORD, wintypes.BOOL, wintypes.LPCWSTR)
    handle = kernel.OpenMutexW(0x00100000, False, 'Local\\MCP_Local_Background')
    if handle:
        kernel.CloseHandle(handle)
        return True
    if ctypes.get_last_error() not in (0, 2):
        raise ValueError('無法確認背景程序狀態。')
    return False


def apply_settings_transaction(settings: dict, previous: dict, save) -> None:
    """排程先驗證及去重；保存失敗時補償本次已確認的排程變更。"""
    desired = settings.get('auto_start', False)
    if not desired and not previous.get('auto_start', False):
        save(settings)
        return
    existed = is_registered()
    if existed and not validate_registered_task():
        raise ValueError('TASK_MISMATCH：同名工作不相符，沒有修改設定或排程。')
    changed = existed != desired
    was_running = background_running() if not desired else False
    if was_running and not existed:
        raise ValueError('TASK_MISMATCH：背景程序仍存在但登入工作遺失，已取消停用。')
    try:
        if was_running:
            stop_background()
        if changed:
            (register_autostart if desired else unregister_autostart)()
        save(settings)
    except Exception as original:
        if changed or was_running:
            try:
                actual = is_registered()
                if actual and not validate_registered_task():
                    raise ValueError()
                if existed and not actual:
                    register_autostart()
                elif not existed and actual:
                    unregister_autostart()
                if was_running:
                    start_background()
            except Exception:
                raise ValueError('SETTINGS_TASK_INCONSISTENT：操作及排程補償失敗，請檢查一般設定與本專案排程。') from None
        raise original
