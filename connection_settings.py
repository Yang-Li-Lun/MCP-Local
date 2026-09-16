"""兩個啟動入口共用一般設定；不讀寫金鑰。"""
from __future__ import annotations
import json
import os
import re
import shutil
import uuid
import hashlib
import threading
import subprocess
from dataclasses import dataclass
from pathlib import Path
from local_files_mcp import FileReader
from workspace_settings import normalize_workspace, encode_workspace
from reader_settings import normalize_reader_settings, encode_reader_settings
from backup_rotation import rotate_backups
from integrity import verify_client

PROJECT = Path(__file__).resolve().parent
CONFIG_DIR = Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local'
CONFIG_FILE = CONFIG_DIR / 'settings.json'
DEFAULT_TUNNEL = 'tunnel_example'


def validate_settings(root: str, tunnel: str) -> dict:
    """沿用伺服器路徑防護；只接受指定格式的通道識別碼。"""
    try:
        reader = FileReader(Path(root))
    except OSError:
        raise ValueError('共享資料夾無法安全存取。') from None
    if CONFIG_DIR.resolve().is_relative_to(reader.root):
        raise ValueError('共享範圍不可包含本程式的一般設定、備份或加密金鑰目錄。')
    if not isinstance(tunnel, str) or not re.fullmatch(r'tunnel_[A-Za-z0-9_-]+', tunnel):
        raise ValueError('通道識別碼必須以 tunnel_ 開頭，且只能包含英數字、底線及連字號。')
    return {'root': str(reader.root), 'tunnel': tunnel}


def normalize_connection(settings: dict, *, validate_paths: bool = True) -> dict:
    """所有入口使用同一具名資料夾與通道驗證。"""
    if not isinstance(settings, dict):
        raise ValueError('設定檔格式錯誤。')
    version = settings.get('settings_version', 0)
    if type(version) is not int or version not in (0, 1, 2, 3):
        raise ValueError('共用設定版本不支援，原檔已保留。')
    reader = normalize_reader_settings(settings.get('reader'))
    if version in (2, 3) and ('roots' not in settings or 'default_root' not in settings):
        raise ValueError('多資料夾設定不完整。')
    roots = settings.get('roots', [{'id': 'main', 'name': 'main', 'path': settings.get('root')}])
    workspace = normalize_workspace(roots, settings.get('default_root', 'main'), reader, validate_paths=validate_paths)
    if not isinstance(settings.get('tunnel'), str) or not re.fullmatch(r'tunnel_[A-Za-z0-9_-]{1,200}', settings['tunnel']):
        raise ValueError('通道識別碼格式錯誤。')
    if validate_paths:
        for item in workspace['roots']:
            validate_settings(item['path'], settings['tunnel'])
    recent = settings.get('recent', [])
    if not isinstance(recent, list) or not all(isinstance(item, str) and len(item) <= 4096 for item in recent):
        raise ValueError('最近使用資料夾格式錯誤。')
    root = next(item['path'] for item in workspace['roots'] if item['id'] == workspace['default_root'])
    for field in ('auto_start', 'auto_connect', 'start_hidden'):
        if type(settings.get(field, False)) is not bool:
            raise ValueError('自動啟動設定必須為布林值。')
    return {**workspace, 'root': root, 'tunnel': settings['tunnel'], 'reader': reader,
            'recent': recent[:8], 'start_hidden': settings.get('start_hidden') is True,
            'auto_start': settings.get('auto_start', False),
            'auto_connect': settings.get('auto_connect', False),
            'settings_version': 3 if version in (1, 2, 3) else 0}


def load_settings(path: Path = CONFIG_FILE) -> dict:
    """舊版已授權設定驗證後備份並遷移；未知版本或無效範圍不改原件。"""
    if not path.exists():
        return normalize_connection({'root': str(PROJECT / 'shared'), 'tunnel': DEFAULT_TUNNEL})
    saved, revision = read_settings_data(path)
    if not isinstance(saved, dict):
        raise ValueError('設定檔格式錯誤。')
    settings = normalize_connection(saved)
    if saved.get('settings_version') in (1, 2):
        save_settings(settings, path, expected_revision=revision)
    return settings


def _write_settings(safe: dict, path: Path, previous: dict | None) -> None:
    if safe == previous:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    if previous is not None:
        backup = path.with_name(path.name + '.' + uuid.uuid4().hex + '.bak')
        # 備份只保留一般設定欄位，排除任何意外混入的金鑰。
        allowed = {'root', 'roots', 'default_root', 'tunnel', 'recent', 'start_hidden', 'reader', 'settings_version', 'auto_start', 'auto_connect'}
        clean = {key: value for key, value in previous.items() if key in allowed}
        if 'roots' in clean:
            clean['roots'] = [{key: value for key, value in item.items()
                               if key in {'id', 'name', 'path', 'excluded_names'}} for item in clean['roots']]
        if 'reader' in clean:
            clean['reader'] = {key: value for key, value in clean['reader'].items()
                               if key in normalize_reader_settings()}
        backup.write_text(json.dumps(clean, ensure_ascii=False, indent=2), encoding='utf-8')
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        temporary.write_text(json.dumps(safe, ensure_ascii=False, indent=2), encoding='utf-8')
        rotate_backups(path, 5, path.parent)
        temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)


MAX_SETTINGS_BYTES = 128 * 1024
SETTINGS_LOCK = threading.RLock()


class SettingsError(ValueError):
    def __init__(self, code: str, message: str):
        self.code = code
        super().__init__(message)


def read_settings_data(path: Path) -> tuple[dict, str]:
    with path.open('rb') as handle:
        data = handle.read(MAX_SETTINGS_BYTES + 1)
    if len(data) > MAX_SETTINGS_BYTES:
        raise SettingsError('SETTINGS_TOO_LARGE', '設定檔超過容量上限，原件已保留。')
    try:
        value = json.loads(data)
        if not isinstance(value, dict):
            raise ValueError()
    except (ValueError, UnicodeError):
        raise SettingsError('SETTINGS_CORRUPT', '設定損毀，原件已保留；請由已知的一般設定備份修復。') from None
    version = value.get('settings_version', 0)
    if type(version) is not int or version not in (0, 1, 2, 3):
        raise SettingsError('SETTINGS_VERSION', '未知設定版本，請使用相容版本；禁止自動覆寫。')
    return value, hashlib.sha256(data).hexdigest()


@dataclass
class EditableSettings:
    settings: dict
    state: str
    errors: list[dict]
    revision: str | None


def load_for_edit(path: Path = CONFIG_FILE) -> EditableSettings:
    """僅解析一般設定；路徑失效仍可修復，不讀取金鑰或遷移原件。"""
    if not path.exists():
        return EditableSettings(load_settings(path), 'CONFIRM_REQUIRED', [], None)
    saved, revision = read_settings_data(path)
    settings = normalize_connection(saved, validate_paths=False)
    errors = []
    for row in settings['roots']:
        try:
            validate_settings(row['path'], settings['tunnel'])
        except (ValueError, OSError):
            errors.append({'root_id': row['id'], 'code': 'ROOT_INVALID'})
    state = 'REPAIR_REQUIRED' if errors else 'READY' if settings['settings_version'] else 'CONFIRM_REQUIRED'
    return EditableSettings(settings, state, errors, revision)


def save_settings(settings: dict, path: Path = CONFIG_FILE, *, expected_revision: str | None = None) -> None:
    """驗證新範圍，結構驗證舊件；以鎖檔及版本核對避免覆蓋另一個更新。"""
    safe = normalize_connection(settings)
    if len(json.dumps(safe, ensure_ascii=False, indent=2).encode('utf-8')) > MAX_SETTINGS_BYTES:
        raise SettingsError('SETTINGS_TOO_LARGE', '一般設定超過容量上限，請縮短設定內容。')
    path.parent.mkdir(parents=True, exist_ok=True)
    lock_path = path.with_name(path.name + '.lock')
    with SETTINGS_LOCK:
        try:
            descriptor = os.open(lock_path, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            raise SettingsError('SETTINGS_BUSY', '另一個設定保存正在執行；若程序異常退出，確認無寫入者後才可移除鎖檔。') from None
        try:
            previous = None
            if path.exists():
                previous, revision = read_settings_data(path)
                normalize_connection(previous, validate_paths=False)
                if expected_revision is not None and revision != expected_revision:
                    raise SettingsError('SETTINGS_CONFLICT', '設定已由另一個操作更新，本次儲存已取消。')
            elif expected_revision not in (None, ''):
                raise SettingsError('SETTINGS_CONFLICT', '原設定已變更，請重新載入。')
            _write_settings(safe, path, previous)
        finally:
            os.close(descriptor)
            lock_path.unlink(missing_ok=True)


def require_migration(settings: dict) -> None:
    """未明確選定分享範圍的歷史設定不得啟動。"""
    if settings.get('settings_version') not in (1, 2, 3):
        raise ValueError('尚未完成共用設定遷移。請開啟連線設定介面，選定分享資料夾及通道後按儲存設定並確認。')


def backup_connection_profile(commands: list[list[str]]) -> None:
    """init --force 前保留原產生的通道設定，不輸出內容。"""
    command = commands[0] if commands else []
    if '--profile-dir' not in command or '--profile' not in command:
        return
    folder = Path(command[command.index('--profile-dir') + 1])
    profile = command[command.index('--profile') + 1]
    if profile != 'local-files-gui':
        raise ValueError('通道設定名稱不符。')
    source = folder / (profile + '.yaml')
    if source.exists():
        if source.is_symlink() or getattr(source.lstat(), 'st_file_attributes', 0) & 0x400:
            raise ValueError('通道設定不可為連結。')
        shutil.copy2(source, source.with_name(source.name + '.' + uuid.uuid4().hex + '.bak'))
        rotate_backups(source, 3, folder)


def build_commands(settings: dict, profile_dir: Path = CONFIG_DIR / 'profiles') -> list[list[str]]:
    """命令透過參數陣列傳遞；不經命令殼層解譯。"""
    settings = normalize_connection(settings)
    verify_client(PROJECT)
    client = PROJECT / 'tunnel-client.exe'
    python = PROJECT / '.venv' / 'Scripts' / 'python.exe'
    server = PROJECT / 'local_files_mcp.py'
    for path in (client, python, server):
        if not path.is_file():
            raise ValueError(f'找不到必要檔案：{path}')
    mcp_command = ' '.join('"' + str(item).replace('\\', '/') + '"'
                           for item in (python, server))
    mcp_command += ' --root "' + settings['root'].replace('\\', '/') + '"'
    mcp_command += ' --workspace-settings ' + encode_workspace(settings)
    mcp_command += ' --reader-settings ' + encode_reader_settings(settings.get('reader'))
    common = ['--profile', 'local-files-gui', '--profile-dir', str(profile_dir)]
    commands = [
        [str(client), 'init', '--force', '--sample', 'sample_mcp_stdio_local',
         *common, '--health-listen-addr', '127.0.0.1:0',
         '--tunnel-id', settings['tunnel'], '--mcp-command', mcp_command],
        [str(client), 'doctor', *common, '--explain'],
        [str(client), 'run', *common],
    ]
    if any(len(subprocess.list2cmdline(command).encode('utf-16-le')) // 2 > 30000 for command in commands):
        raise ValueError('連線命令超過安全長度上限，請縮短路徑或設定清單。')
    return commands
