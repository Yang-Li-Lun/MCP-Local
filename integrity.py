"""以固定清單檢查供應商成品；不下載或自動信任新檔案。"""
import hashlib
import json
import re
from pathlib import Path


class IntegrityError(ValueError):
    """供應商成品或完整性清單無效。"""


def verify_client(project: Path) -> None:
    try:
        manifest = json.loads((project / 'vendor-integrity.json').read_text(encoding='utf-8'))
        expected = manifest['tunnel-client.exe']['sha256']
        if not isinstance(expected, str) or not re.fullmatch('[0-9a-f]{64}', expected):
            raise ValueError()
        path = project / 'tunnel-client.exe'
        info = path.lstat()
        if path.is_symlink() or info.st_nlink > 1 or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError()
        digest = hashlib.sha256()
        with path.open('rb') as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b''):
                digest.update(chunk)
        if digest.hexdigest() != expected:
            raise ValueError()
    except (OSError, ValueError, KeyError, TypeError):
        raise IntegrityError('通道程式完整性驗證失敗，請核對官方成品與完整性清單。') from None
