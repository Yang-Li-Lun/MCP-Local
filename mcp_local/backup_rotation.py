"""只輪替指定設定目錄內、固定 UUID 名稱的備份。"""
import re
from pathlib import Path


def rotate_backups(base: Path, keep: int, allowed_dir: Path) -> None:
    if base.parent.resolve() != allowed_dir.resolve() or base.name not in ('settings.json', 'local-files-gui.yaml'):
        raise ValueError('備份範圍不符。')
    if allowed_dir.is_symlink() or getattr(allowed_dir.lstat(), 'st_file_attributes', 0) & 0x400:
        raise ValueError('備份目錄不可為連結。')
    pattern = re.compile(re.escape(base.name) + r'\.[0-9a-f]{32}\.bak')
    backups = []
    for path in allowed_dir.iterdir():
        if not pattern.fullmatch(path.name):
            continue
        info = path.lstat()
        if path.is_symlink() or info.st_nlink > 1 or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError('備份不可為連結，已保留。')
        if not path.is_file():
            raise ValueError('備份格式錯誤。')
        backups.append((info.st_mtime_ns, path.name, path))
    for _, _, path in sorted(backups, reverse=True)[keep:]:
        path.unlink()
