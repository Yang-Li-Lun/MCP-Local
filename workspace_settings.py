"""具名共享資料夾的共用驗證與無金鑰快照。"""
from __future__ import annotations
import base64
import json
import re
from pathlib import Path
from reader_settings import normalize_reader_settings

MAX_ROOTS = 8


def normalize_workspace(roots: list, default_root: str, settings: dict | None = None, *, validate_paths: bool = True) -> dict:
    from local_files_mcp import FileReader
    if not isinstance(roots, list) or not 1 <= len(roots) <= MAX_ROOTS:
        raise ValueError('共享資料夾必須有 1 至 8 筆。')
    result = []
    ids = set()
    for item in roots:
        if not isinstance(item, dict) or set(item) - {'id', 'name', 'path', 'excluded_names'}:
            raise ValueError('共享資料夾設定格式錯誤。')
        identifier = item.get('id')
        if not isinstance(identifier, str) or not re.fullmatch(r'[A-Za-z][A-Za-z0-9_-]{0,31}', identifier):
            raise ValueError('資料夾代號須以英文字母開頭，最多 32 個英數字、底線或連字號。')
        if identifier.casefold() in ids:
            raise ValueError('資料夾代號不可重複。')
        ids.add(identifier.casefold())
        name = item.get('name', identifier)
        if not isinstance(name, str) or not name.strip() or len(name) > 80 or any(ord(c) < 32 for c in name):
            raise ValueError('資料夾名稱須為 1 至 80 字元。')
        path = item.get('path')
        if not isinstance(path, str) or len(path) > 4096 or not Path(path).is_absolute():
            raise ValueError('設定中的資料夾必須使用絕對路徑。')
        exclusions = normalize_reader_settings({'excluded_names': item.get('excluded_names', [])})['excluded_names']
        effective = normalize_reader_settings(settings)
        effective['excluded_names'] = sorted(set(effective['excluded_names']) | set(exclusions))
        effective = normalize_reader_settings(effective)
        try:
            normalized_path = FileReader(Path(path), effective).root if validate_paths else Path(path)
        except (OSError, ValueError):
            raise ValueError('共享資料夾不存在或未通過安全驗證。') from None
        for previous in result:
            other = Path(previous['path'])
            if normalized_path.is_relative_to(other) or other.is_relative_to(normalized_path):
                raise ValueError('共享資料夾不可重複或互相包含，請改用彼此獨立的專用資料夾。')
        result.append({'id': identifier, 'name': name.strip(), 'path': str(normalized_path),
                       'excluded_names': exclusions})
    if not isinstance(default_root, str) or default_root not in [item['id'] for item in result]:
        raise ValueError('預設資料夾代號不存在。')
    return {'roots': result, 'default_root': default_root}


def encode_workspace(value: dict) -> str:
    safe = normalize_workspace(value['roots'], value['default_root'], value.get('reader'))
    encoded = base64.b64encode(json.dumps(safe, ensure_ascii=False).encode('utf-8')).decode('ascii')
    if len(encoded) > 65536:
        raise ValueError('共享資料夾快照超過容量上限。')
    return encoded


def decode_workspace(value: str, settings: dict | None = None) -> dict:
    try:
        if len(value) > 65536:
            raise ValueError()
        decoded = json.loads(base64.b64decode(value, validate=True))
        return normalize_workspace(decoded['roots'], decoded['default_root'], settings)
    except (ValueError, KeyError, TypeError, UnicodeError):
        raise ValueError('共享資料夾快照無效。') from None
