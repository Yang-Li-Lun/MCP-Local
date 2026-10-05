"""主機控制：命令受信任；檔案 API 仍保留保護路徑、雜湊及大小限制。"""
from __future__ import annotations

import threading
import time
from collections import OrderedDict
from typing import Annotated, Literal
from pydantic import Field

from access_mode import HOST_CONTROL, normalize_access_mode
from control_sessions import Sessions, Session, MAX_SESSIONS, MAX_ACTIVE, SESSION_TTL
from full_control import FullControl, RelativePath, Sha256, Text, SessionId
from host_files import HostFiles
from host_windows import HostProcess


class HostSessions(Sessions):
    def require_mode(self) -> None:
        if normalize_access_mode(self.mode) != HOST_CONTROL:
            raise ValueError('HOST_MODE_REQUIRED：需要本機明確授權的主機模式。')

    def start_host(self, command: str, working_directory: str | None, root_id: str | None,
                   timeout_ms: int, lifetime_seconds: int) -> dict:
        self.require_mode()
        if not command.strip() or '\0' in command or len(command) > 8192:
            raise ValueError('COMMAND_LIMIT：命令必須為 1 至 8192 字元。')
        _, cwd = self.files.checked_path(working_directory or '.', root_id)
        if not cwd.is_dir():
            raise ValueError('HOST_CWD：工作路徑必須為既有目錄。')
        with self.lock:
            if self.closed:
                raise ValueError('SESSION_CLOSED：服務已關閉。')
            for key, item in list(self.sessions.items()):
                if item.finished and time.monotonic() - item.finished > SESSION_TTL:
                    self.close_session(key)
            if len(self.sessions) >= MAX_SESSIONS or sum(not s.done.is_set() for s in self.sessions.values()) >= MAX_ACTIVE:
                raise ValueError('SESSION_LIMIT：session 名額已滿，請先關閉已完成的 session。')
            process = HostProcess(cwd)
            try:
                process.start(command)
            except BaseException:
                process.close()
                raise
            session = Session(process, backend='host_process', host_files_access='current_user_trusted_commands')
            self.sessions[session.identifier] = session
            drain = threading.Thread(target=self._drain, args=(session,), daemon=True)
            monitor = threading.Thread(target=self._monitor, args=(session, drain, lifetime_seconds), daemon=True)
            drain.start()
            monitor.start()
        session.done.wait(timeout_ms / 1000)
        return session.view()


class HostControl(FullControl):
    def __init__(self, workspace, mode: str = HOST_CONTROL):
        if normalize_access_mode(mode) != HOST_CONTROL:
            raise ValueError('HOST_MODE_REQUIRED：需要本機明確授權的主機模式。')
        # Refuse elevated service startup before registering any host capability.
        process = HostProcess(workspace.reader(None).root)
        process.close()
        self.files = HostFiles(workspace, mode)
        self.sessions = HostSessions(self.files, mode)
        self._directory_readers = OrderedDict()

    def write_file(self, path: RelativePath, content: Text,
                   mode: Literal['create', 'rewrite', 'append'] = 'create',
                   expected_sha256: Sha256 | None = None, root_id: str | None = None) -> dict:
        """主機 UTF-8 寫入：可指定本機絕對路徑；相對路徑使用 root_id。保留保護路徑與 SHA-256 防護。"""
        return self.files.write(path, content, mode, expected_sha256, root_id)

    def create_directory(self, path: RelativePath, root_id: str | None = None) -> dict:
        """主機既有允許父目錄內建立一層目錄；可指定本機絕對路徑。"""
        return self.files.mkdir(path, root_id)

    def move_file(self, source: RelativePath, destination: RelativePath,
                  expected_sha256: Sha256, root_id: str | None = None) -> dict:
        """同一磁碟內移動一般檔案；可跨原共享根，核對 SHA-256、不覆寫目的地。"""
        return self.files.move(source, destination, expected_sha256, root_id)

    def start_process(self, command: Annotated[str, Field(strict=True, min_length=1, max_length=8192)],
                      working_directory: RelativePath | None = None, root_id: str | None = None,
                      timeout_ms: Annotated[int, Field(strict=True, ge=0, le=3000)] = 1000,
                      lifetime_seconds: Annotated[int, Field(strict=True, ge=1, le=3600)] = 900) -> dict:
        """執行受信任的主機 PowerShell，可使用已安裝 CLI、跨共享根。不是沙箱。

        不自動提升權限；工作目錄不是安全邊界。命令可存取目前使用者的檔案與憑證，
        不受 MCP 檔案 API 的保護路徑限制。只繼承明列的一般環境變數。
        程序與直接衍生子程序納入 Job，逾時／關閉服務時停止；不要啟動外部常駐服務。
        """
        return self.sessions.start_host(command, working_directory, root_id, timeout_ms, lifetime_seconds)

    def read_host_file(self, path: RelativePath,
                       start_line: Annotated[int, Field(strict=True, ge=1)] = 1,
                       line_count: Annotated[int, Field(strict=True, ge=1, le=400)] = 200,
                       root_id: str | None = None) -> dict:
        """讀取主機 UTF-8 文字，可指定絕對路徑；保留 MCP 狀態、隱藏、連結與排除規則。"""
        with self.files.lock:
            reader, target = self.files.checked_path(path, root_id)
            result = reader.read_file(reader.relative(target), start_line, line_count)
            return {**result, 'path': str(target)}

    def list_host_directory(self, directory: RelativePath,
                            limit: Annotated[int, Field(strict=True, ge=1, le=500)] = 200,
                            cursor: Annotated[str, Field(strict=True, max_length=16000)] | None = None,
                            root_id: str | None = None) -> dict:
        """分頁列主機絕對目錄第一層；保留 MCP 狀態、隱藏、連結與排除規則。最多保留 32 個目錄游標身分。"""
        with self.files.lock:
            absolute = self.files.absolute(directory, root_id)
            key = (absolute.casefold(), root_id)
            reader = self._directory_readers.get(key)
            if reader is None:
                reader, target = self.files.checked_path(directory, root_id)
                if not target.is_dir():
                    raise ValueError('HOST_DIRECTORY：需要既有目錄。')
                self._directory_readers[key] = reader
                if len(self._directory_readers) > 32:
                    self._directory_readers.popitem(last=False)
            self._directory_readers.move_to_end(key)
            return {**reader.list_directory('.', limit, cursor), 'directory': absolute}

    def export_session_file(self, session_id: SessionId, path: RelativePath,
                            destination: RelativePath, expected_sha256: Sha256 | None = None,
                            root_id: str | None = None) -> dict:
        """session 停止後複製其工作目錄中的單一文字檔；目的地可為受保護規則允許的主機絕對路徑。"""
        return self.sessions.export(session_id, path, destination, expected_sha256, root_id)

    def close_session(self, session_id: SessionId) -> dict:
        """停止此 session 的 Job 並釋放名額；主機工作目錄及產出檔案保留。"""
        return self.sessions.close_session(session_id)

    def force_terminate(self, session_id: SessionId) -> dict:
        """終止此主機 session 的 Job 與直接衍生子程序，保留輸出及主機產出檔案。"""
        return self.sessions.terminate(session_id)
