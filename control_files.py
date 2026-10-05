"""有界共享根寫入；保留既有排除，鎖住 Windows 路徑祖先以阻止替換。"""
from __future__ import annotations

from contextlib import contextmanager, ExitStack
import ctypes as c
from ctypes import wintypes as w
import hashlib
import os
from pathlib import Path, PureWindowsPath
import stat
import threading

from access_mode import require_full_control
from control_edit import replace_block
from local_files_mcp import linked, hidden
from operation_budget import checkpoint
from security_policy import validate_read_policy

MAX_WRITE_BYTES = 2 * 1024 * 1024
PROJECT = Path(__file__).resolve().parent


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


@contextmanager
def locked_path(path: Path, *, directory: bool = False):
    """本版寫入僅支援 Windows；不提供不安全的跨平台 fallback。"""
    if os.name != 'nt':
        raise ValueError('CONTROL_PLATFORM：完整控制寫入需要 Windows。')
    kernel = c.WinDLL('kernel32', use_last_error=True)
    create = kernel.CreateFileW
    create.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, c.c_void_p, w.DWORD, w.DWORD, w.HANDLE]
    create.restype = w.HANDLE
    close = kernel.CloseHandle
    close.argtypes, close.restype = [w.HANDLE], w.BOOL
    # Deny directory deletion/rename while allowing child file operations.
    # As with FileReader, trusted local writers remain a deployment requirement.
    handle = create(str(path), 0x80000000, 3 if directory else 1, None, 3,
                    0x00200000 | (0x02000000 if directory else 0), None)
    if handle == c.c_void_p(-1).value:
        raise ValueError('PATH_BUSY：路徑無法鎖定；請關閉寫入者後重試。')
    try:
        if linked(path) or not directory and hidden(path):
            raise ValueError('路徑已變更為隱藏或連結。')
        yield handle
    finally:
        close(handle)


class ControlledFiles:
    def __init__(self, workspace, mode: str):
        self.workspace = workspace
        self.mode = mode
        self.lock = threading.RLock()
        self.closed = False

    def close(self) -> None:
        """等待已進入寫入區段的操作完成，再永久撤銷此物件的寫入授權。"""
        with self.lock:
            self.closed = True

    def require_active(self) -> None:
        require_full_control(self.mode)
        if self.closed:
            raise ValueError('CONTROL_CLOSED：此服務的寫入授權已撤銷，請使用目前連線。')

    def reader(self, root_id: str | None):
        with self.lock:
            self.require_active()
        return self.workspace.reader(root_id)

    def reader_for_path(self, path: str, root_id: str | None):
        return self.reader(root_id)

    def target(self, reader, relative: str) -> Path:
        if (not isinstance(relative, str) or not relative or len(relative) > 4096 or
                any(ord(ch) < 32 for ch in relative) or ':' in relative):
            raise ValueError('請使用共享根內的相對路徑。')
        raw = Path(relative.replace('\\', '/'))
        win = PureWindowsPath(relative)
        if raw.is_absolute() or win.root or win.drive or '..' in raw.parts or not raw.parts:
            raise ValueError('不允許根目錄、絕對路徑或向上跳出共享根。')
        for part in raw.parts:
            if (part.startswith('.') or part.casefold() in reader._exclusions or
                    part.endswith((' ', '.')) or PureWindowsPath(part).is_reserved() or
                    any(ch in '*?"<>|' for ch in part)):
                raise ValueError('此名稱已被安全或產物排除規則封鎖。')
        parent = reader.checked(raw.parent.as_posix())
        if not parent.is_dir():
            raise ValueError('父路徑必須為現有目錄。')
        target = parent / raw.name
        validate_read_policy(target)
        # The control plane must never rewrite itself or move its ancestors.
        if target.is_relative_to(PROJECT) or PROJECT.is_relative_to(target):
            raise ValueError('CONTROL_PLANE_PROTECTED：禁止修改服務程式及其祖先。')
        if os.path.lexists(target):
            reader.checked(relative)
            if getattr(target.stat(), 'st_file_attributes', 0) & 1:
                raise ValueError('來源具有唯讀屬性。')
        return target

    @contextmanager
    def guarded(self, reader, *relative: str):
        with self.lock, ExitStack() as stack:
            # Recheck under the mutation lock: a queued request may have obtained
            # its reader before service shutdown revoked this controller.
            self.require_active()
            targets = [self.target(reader, value) for value in relative]
            parents = {node for target in targets for node in target.parents}
            # Include ancestors above the shared root, so the root cannot be moved.
            for parent in sorted(parents, key=lambda p: len(p.parts)):
                stack.enter_context(locked_path(parent, directory=True))
            targets = [self.target(reader, value) for value in relative]
            yield targets

    def read_bytes(self, reader, path: Path) -> bytes:
        with reader.open_checked(path, binary=True) as source:
            raw = source.read(min(MAX_WRITE_BYTES, reader.settings['max_file_bytes']) + 1)
        if len(raw) > min(MAX_WRITE_BYTES, reader.settings['max_file_bytes']):
            raise ValueError('WRITE_LIMIT：檔案超過寫入上限。')
        return raw

    def write(self, path: str, content: str, mode: str, expected_sha256: str | None,
              root_id: str | None) -> dict:
        reader = self.reader_for_path(path, root_id)
        if mode not in ('create', 'rewrite', 'append') or not isinstance(content, str) or '\0' in content:
            raise ValueError('WRITE_INVALID：模式或 UTF-8 內容無效。')
        with self.guarded(reader, path) as (target,):
            if not reader.is_text(target):
                raise ValueError('只允許寫入已授權的文字副檔名。')
            original = None
            if target.exists():
                if mode == 'create':
                    raise ValueError('FILE_EXISTS：建立模式不可覆寫檔案。')
                original = self.read_bytes(reader, target)
                if expected_sha256 != digest(original):
                    raise ValueError('CONTENT_CONFLICT：必須提供目前檔案的 SHA-256。')
            elif mode != 'create' or expected_sha256 is not None:
                raise ValueError('FILE_MISSING：請使用 create 模式建立新檔。')
            raw = content.encode('utf-8')
            if mode == 'append':
                original.decode('utf-8-sig')
                raw = original + raw
            return self.commit(reader, target, raw, original)

    def commit(self, reader, target: Path, raw: bytes, original: bytes | None) -> dict:
        if len(raw) > min(MAX_WRITE_BYTES, reader.settings['max_file_bytes']):
            raise ValueError('WRITE_LIMIT：寫入結果超過 2 MiB 或讀取設定上限。')
        checkpoint()
        # Preserve destination ACLs and prevent concurrent writes. The complete old
        # bytes are retained for rollback if a write/flush fails. No shell is used.
        if original is None:
            with target.open('xb') as output:
                try:
                    output.write(raw)
                    output.flush()
                    os.fsync(output.fileno())
                except BaseException:
                    output.close()
                    target.unlink()
                    raise
        else:
            import msvcrt
            kernel = c.WinDLL('kernel32', use_last_error=True)
            create = kernel.CreateFileW
            create.argtypes = [w.LPCWSTR, w.DWORD, w.DWORD, c.c_void_p, w.DWORD, w.DWORD, w.HANDLE]
            create.restype = w.HANDLE
            handle = create(str(target), 0xc0000000, 0, None, 3, 0x00200000, None)
            if handle == c.c_void_p(-1).value:
                raise ValueError('FILE_BUSY：檔案被占用，未寫入。')
            descriptor = msvcrt.open_osfhandle(handle, os.O_RDWR | os.O_BINARY)
            with os.fdopen(descriptor, 'r+b') as output:
                info = os.fstat(output.fileno())
                if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or
                        getattr(info, 'st_file_attributes', 0) & 0x403 or
                        output.read(len(original) + 1) != original):
                    raise ValueError('CONTENT_CONFLICT：檔案或安全屬性已變更。')
                try:
                    output.seek(0)
                    output.write(raw)
                    output.truncate()
                    output.flush()
                    os.fsync(output.fileno())
                except BaseException:
                    output.seek(0)
                    output.write(original)
                    output.truncate()
                    output.flush()
                    os.fsync(output.fileno())
                    raise
        return {'path': reader.relative(target), 'bytes': len(raw), 'sha256': digest(raw)}

    def edit(self, path: str, old: str, new: str, expected: int, sha256: str,
             root_id: str | None) -> dict:
        reader = self.reader_for_path(path, root_id)
        with self.guarded(reader, path) as (target,):
            if not reader.is_text(target):
                raise ValueError('只允許編輯已授權的文字副檔名。')
            original = self.read_bytes(reader, target)
            if digest(original) != sha256:
                raise ValueError('CONTENT_CONFLICT：SHA-256 已變更。')
            text = original.decode('utf-8-sig')
            if '\0' in text or '\0' in old or '\0' in new:
                raise ValueError('不支援 NUL 文字。')
            updated = replace_block(text, old, new, expected).encode('utf-8')
            if original.startswith(b'\xef\xbb\xbf'):
                updated = b'\xef\xbb\xbf' + updated
            return {**self.commit(reader, target, updated, original), 'replacements': expected}

    def mkdir(self, path: str, root_id: str | None) -> dict:
        reader = self.reader_for_path(path, root_id)
        with self.guarded(reader, path) as (target,):
            target.mkdir()
            return {'path': reader.relative(target), 'created': True}

    def move(self, source: str, destination: str, sha256: str, root_id: str | None) -> dict:
        reader = self.reader(root_id)
        with self.guarded(reader, source, destination) as (src, dst):
            if os.path.lexists(dst):
                raise ValueError('FILE_EXISTS：移動不覆寫現有路徑。')
            if digest(self.read_bytes(reader, src)) != sha256:
                raise ValueError('CONTENT_CONFLICT：SHA-256 已變更。')
            # Windows rename never overwrites an existing destination.
            src.rename(dst)
            return {'source': reader.relative(src), 'destination': reader.relative(dst), 'sha256': sha256}

    def delete(self, path: str, sha256: str, root_id: str | None) -> dict:
        reader = self.reader_for_path(path, root_id)
        with self.guarded(reader, path) as (target,):
            if digest(self.read_bytes(reader, target)) != sha256:
                raise ValueError('CONTENT_CONFLICT：SHA-256 已變更。')
            target.unlink()
            return {'path': reader.relative(target), 'deleted': True, 'sha256': sha256}
