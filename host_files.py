"""主機模式檔案 API：絕對本機路徑，保留既有 FileReader 與寫入防護。"""
from __future__ import annotations

from pathlib import Path, PureWindowsPath
import os

from access_mode import HOST_CONTROL, normalize_access_mode
from control_files import ControlledFiles, digest
from local_files_mcp import FileReader


class HostFiles(ControlledFiles):
    def require_active(self) -> None:
        if normalize_access_mode(self.mode) != HOST_CONTROL:
            raise ValueError('HOST_MODE_REQUIRED：需要本機明確授權的主機模式。')
        if self.closed:
            raise ValueError('CONTROL_CLOSED：此服務的寫入授權已撤銷，請使用目前連線。')

    def absolute(self, path: str, root_id: str | None = None) -> str:
        self.require_active()
        if not isinstance(path, str) or not path or len(path) > 4096 or any(ord(ch) < 32 for ch in path):
            raise ValueError('HOST_PATH：路徑格式無效。')
        raw = PureWindowsPath(path)
        if ('..' in raw.parts or raw.drive.startswith('\\\\') or
                (raw.drive and not raw.is_absolute()) or (raw.root and not raw.drive)):
            raise ValueError('HOST_PATH：拒絕 UNC、裝置路徑、磁碟相對路徑及向上跳轉。')
        if raw.is_absolute():
            if len(raw.drive) != 2 or raw.drive[1] != ':' or not raw.drive[0].isalpha():
                raise ValueError('HOST_PATH：僅接受本機磁碟的絕對路徑。')
            result = Path(path)
        else:
            result = self.workspace.reader(root_id).root / path
        if ':' in str(result)[2:]:
            raise ValueError('HOST_PATH：拒絕替代資料串流。')
        exclusions = self.workspace.reader(root_id)._exclusions
        for part in result.parts[1:]:
            if (part.startswith('.') or part.casefold() in exclusions or part.endswith((' ', '.'))
                    or PureWindowsPath(part).is_reserved() or any(ch in '*?"<>|' for ch in part)):
                raise ValueError('HOST_PATH：路徑包含隱藏、排除或含糊名稱。')
        return str(result)

    def reader_for_path(self, path: str, root_id: str | None) -> FileReader:
        self.require_active()
        selected = self.workspace.reader(root_id)
        absolute = Path(self.absolute(path, root_id))
        return FileReader(absolute.parent, selected.settings)

    def target(self, reader: FileReader, relative: str) -> Path:
        absolute = Path(relative)
        if absolute.is_absolute():
            try:
                relative = absolute.relative_to(reader.root).as_posix()
            except ValueError:
                raise ValueError('HOST_MOVE_VOLUME：移動僅支援同一磁碟。') from None
        return super().target(reader, relative)

    def checked_path(self, path: str, root_id: str | None = None) -> tuple[FileReader, Path]:
        absolute = Path(self.absolute(path, root_id))
        if absolute.is_dir():
            reader = FileReader(absolute, self.workspace.reader(root_id).settings)
            return reader, reader.checked('.')
        reader = self.reader_for_path(str(absolute), root_id)
        return reader, reader.checked(absolute.relative_to(reader.root).as_posix())

    def write(self, path: str, content: str, mode: str, expected_sha256: str | None,
              root_id: str | None) -> dict:
        absolute = self.absolute(path, root_id)
        result = super().write(absolute, content, mode, expected_sha256, root_id)
        return {**result, 'path': absolute}

    def edit(self, path: str, old: str, new: str, expected: int, sha256: str,
             root_id: str | None) -> dict:
        absolute = self.absolute(path, root_id)
        result = super().edit(absolute, old, new, expected, sha256, root_id)
        return {**result, 'path': absolute}

    def mkdir(self, path: str, root_id: str | None) -> dict:
        absolute = self.absolute(path, root_id)
        return {**super().mkdir(absolute, root_id), 'path': absolute}

    def move(self, source: str, destination: str, sha256: str, root_id: str | None) -> dict:
        source, destination = self.absolute(source, root_id), self.absolute(destination, root_id)
        if Path(source).drive.casefold() != Path(destination).drive.casefold():
            raise ValueError('HOST_MOVE_VOLUME：移動僅支援同一磁碟。')
        source_reader = self.reader_for_path(source, root_id)
        destination_reader = self.reader_for_path(destination, root_id)
        with self.guarded(source_reader, source) as (src,):
            with self.guarded(destination_reader, destination) as (dst,):
                if os.path.lexists(dst):
                    raise ValueError('FILE_EXISTS：移動不覆寫現有路徑。')
                if digest(self.read_bytes(source_reader, src)) != sha256:
                    raise ValueError('CONTENT_CONFLICT：SHA-256 已變更。')
                src.rename(dst)
                return {'source': source, 'destination': destination, 'sha256': sha256}

    def delete(self, path: str, sha256: str, root_id: str | None) -> dict:
        absolute = self.absolute(path, root_id)
        return {**super().delete(absolute, sha256, root_id), 'path': absolute}
