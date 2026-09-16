"""兩個啟動入口共用一般設定；不讀寫金鑰。"""
from __future__ import annotations
import json
import os
import re
import shutil
import uuid
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


def normalize_connection(settings: dict) -> dict:
    """所有入口使用同一具名資料夾與通道驗證。"""
    version = settings.get('settings_version', 0)
    if type(version) is not int or version not in (0, 1, 2, 3):
        raise ValueError('共用設定版本不支援，原檔已保留。')
    reader = normalize_reader_settings(settings.get('reader'))
    if version in (2, 3) and ('roots' not in settings or 'default_root' not in settings):
        raise ValueError('多資料夾設定不完整。')
    roots = settings.get('roots', [{'id': 'main', 'name': 'main', 'path': settings.get('root')}])
    workspace = normalize_workspace(roots, settings.get('default_root', 'main'), reader)
    for item in workspace['roots']:
        validate_settings(item['path'], settings.get('tunnel', ''))
    recent = settings.get('recent', [])
    if not isinstance(recent, list) or not all(isinstance(item, str) for item in recent):
        raise ValueError('最近使用資料夾格式錯誤。')
    root = next(item['path'] for item in workspace['roots'] if item['id'] == workspace['default_root'])
    for field in ('auto_start', 'auto_connect'):
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
    saved = json.loads(path.read_text(encoding='utf-8'))
    if not isinstance(saved, dict):
        raise ValueError('設定檔格式錯誤。')
    settings = normalize_connection(saved)
    if saved.get('settings_version') in (1, 2):
        _write_settings(settings, path, saved)
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
        backup.write_text(json.dumps(clean, ensure_ascii=False, indent=2), encoding='utf-8')
    temporary = path.with_name(path.name + '.' + uuid.uuid4().hex + '.tmp')
    try:
        temporary.write_text(json.dumps(safe, ensure_ascii=False, indent=2), encoding='utf-8')
        temporary.replace(path)
        rotate_backups(path, 5, path.parent)
    finally:
        temporary.unlink(missing_ok=True)


def save_settings(settings: dict, path: Path = CONFIG_FILE) -> None:
    """先驗證新舊設定，再保存不含金鑰的備份並原子替換。"""
    safe = normalize_connection(settings)
    previous = None
    if path.exists():
        previous = json.loads(path.read_text(encoding='utf-8'))
        if not isinstance(previous, dict):
            raise ValueError('設定檔格式錯誤。')
        normalize_connection(previous)
    _write_settings(safe, path, previous)


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
    return [
        [str(client), 'init', '--force', '--sample', 'sample_mcp_stdio_local',
         *common, '--health-listen-addr', '127.0.0.1:0',
         '--tunnel-id', settings['tunnel'], '--mcp-command', mcp_command],
        [str(client), 'doctor', *common, '--explain'],
        [str(client), 'run', *common],
    ]
