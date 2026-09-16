"""GUI 與唯讀伺服器共用的讀取設定與驗證。"""
from __future__ import annotations

import base64
import json
import re

MIB = 1024 * 1024
DEFAULT_EXTENSIONS = {
    '.txt', '.md', '.rst', '.kt', '.kts', '.java', '.gradle', '.xml',
    '.json', '.yaml', '.yml', '.toml', '.ini', '.cfg', '.csv', '.tsv',
    '.py', '.js', '.jsx', '.ts', '.tsx', '.html', '.css', '.scss',
    '.c', '.h', '.cpp', '.hpp', '.cs', '.go', '.rs', '.sql', '.pine',
    '.sh', '.ps1', '.bat', '.log', '.properties',
}
DEFAULT_NAMES = {'readme', 'license', 'dockerfile', 'makefile', 'cmakelists.txt', 'go.mod'}
DEFAULT_EXCLUSIONS = {
    'node_modules', 'build', 'dist', 'target', '__pycache__', 'venv',
    'local.properties', 'gradle.properties', 'credentials.json',
    'secrets.json', 'secrets.yaml', 'secrets.yml', 'id_rsa', 'id_ed25519',
}
NUMERIC_FIELDS = {
    'max_file_bytes': ('單檔讀取上限', 2 * MIB, 64 * MIB),
    'max_scan_files': ('掃描檔案上限', 3000, 100000),
    'max_scan_entries': ('掃描項目上限', 12000, 300000),
    'max_scan_bytes': ('搜尋累計讀取上限', 32 * MIB, 1024 * MIB),
}
LIST_FIELDS = {
    'extensions': ('允許副檔名', DEFAULT_EXTENSIONS),
    'text_names': ('允許完整檔名', DEFAULT_NAMES),
    'excluded_names': ('排除名稱', DEFAULT_EXCLUSIONS),
}


def normalize_reader_settings(value: dict | None = None) -> dict:
    """補上舊設定缺少的欄位，拒絕無效值及過大的設定。"""
    if value is None:
        value = {}
    if not isinstance(value, dict):
        raise ValueError('進階設定必須是物件。')
    unknown = set(value) - set(NUMERIC_FIELDS) - set(LIST_FIELDS)
    if unknown:
        raise ValueError('進階設定包含不支援的欄位。')
    result = {}
    for key, (label, default, maximum) in NUMERIC_FIELDS.items():
        number = value.get(key, default)
        if type(number) is not int or not 1 <= number <= maximum:
            raise ValueError(f'{label}必須是 1 至 {maximum:,} 的整數（容量以 bytes 計）。')
        result[key] = number
    for key, (label, default) in LIST_FIELDS.items():
        entries = value.get(key, sorted(default))
        if not isinstance(entries, list) or len(entries) > 128:
            raise ValueError(f'{label}必須是清單，最多 128 項。')
        normalized = set()
        for entry in entries:
            if not isinstance(entry, str):
                raise ValueError(f'{label}只能包含文字。')
            entry = entry.strip().casefold()
            if not entry or len(entry) > 80 or entry in ('.', '..') or re.search(r'[\\/:*?"<>|\x00-\x1f]', entry):
                raise ValueError(f'{label}請填單一名稱，不使用路徑或萬用字元。')
            if key == 'extensions':
                entry = '.' + entry.lstrip('.')
                if not re.fullmatch(r'\.[\w+-]+', entry):
                    raise ValueError('副檔名格式不正確，範例：.txt、.md、.py。')
            normalized.add(entry)
        result[key] = sorted(normalized)
    if len(json.dumps(result, ensure_ascii=False).encode('utf-8')) > 12000:
        raise ValueError('進階設定文字總量過大，請縮短名稱清單。')
    return result


def encode_reader_settings(value: dict | None = None) -> str:
    """把本次連線的設定快照放入命令參數，不受後續儲存影響。"""
    data = json.dumps(normalize_reader_settings(value), ensure_ascii=False).encode('utf-8')
    return base64.b64encode(data).decode('ascii')


def decode_reader_settings(value: str) -> dict:
    """解析本機啟動器傳入的設定，損毀時拒絕啟動。"""
    if len(value) > 16000:
        raise ValueError('進階設定文字總量過大。')
    try:
        return normalize_reader_settings(json.loads(base64.b64decode(value, validate=True)))
    except (ValueError, UnicodeError) as exc:
        raise ValueError('進階設定無效：' + str(exc)) from exc
