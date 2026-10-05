"""完整控制工具；檔案沿用共享根，命令只在 Windows AppContainer 執行。"""
from typing import Annotated, Literal
from pydantic import Field
import functools

from access_mode import FULL_CONTROL, require_full_control
from control_files import ControlledFiles
from control_sessions import Sessions

RelativePath = Annotated[str, Field(strict=True, min_length=1, max_length=4096)]
Sha256 = Annotated[str, Field(strict=True, pattern='^[0-9a-f]{64}$')]
SessionId = Annotated[str, Field(strict=True, pattern='^[0-9a-f]{32}$')]
Text = Annotated[str, Field(strict=True, max_length=2 * 1024 * 1024)]


def safe_control_tool(function):
    @functools.wraps(function)
    def invoke(*args, **kwargs):
        try:
            return function(*args, **kwargs)
        except (OSError, UnicodeError):
            raise ValueError('CONTROL_IO：無法安全存取檔案、格式不符或路徑被占用。') from None
    return invoke


class FullControl:
    def __init__(self, workspace, mode: str = FULL_CONTROL):
        require_full_control(mode)
        self.files = ControlledFiles(workspace, mode)
        self.sessions = Sessions(self.files, mode)

    def write_file(self, path: RelativePath, content: Text,
                   mode: Literal['create', 'rewrite', 'append'] = 'create',
                   expected_sha256: Sha256 | None = None, root_id: str | None = None) -> dict:
        """寫入共享根 UTF-8 文字。建立不覆寫；rewrite/append 必須提供原檔 SHA-256。"""
        return self.files.write(path, content, mode, expected_sha256, root_id)

    def edit_block(self, path: RelativePath, old_string: Text, new_string: Text,
                   expected_sha256: Sha256,
                   expected_replacements: Annotated[int, Field(strict=True, ge=1, le=1000)] = 1,
                   root_id: str | None = None) -> dict:
        """精確區塊替換；保留 BOM/原有換行。次數或 SHA-256 不符就拒絕，不做模糊寫入。"""
        return self.files.edit(path, old_string, new_string, expected_replacements, expected_sha256, root_id)

    def create_directory(self, path: RelativePath, root_id: str | None = None) -> dict:
        """在既有允許目錄內建立一層新目錄；不自動建立祖先。"""
        return self.files.mkdir(path, root_id)

    def move_file(self, source: RelativePath, destination: RelativePath,
                  expected_sha256: Sha256, root_id: str | None = None) -> dict:
        """同一共享根內移動單一一般檔案，不覆寫目的地，不搬動目錄樹。"""
        return self.files.move(source, destination, expected_sha256, root_id)

    def delete_file(self, path: RelativePath, expected_sha256: Sha256, root_id: str | None = None) -> dict:
        """以 SHA-256 核對後刪除單一一般檔案；不刪目錄或遞迴刪除。"""
        return self.files.delete(path, expected_sha256, root_id)

    def start_process(self, command: Annotated[str, Field(strict=True, min_length=1, max_length=8192)],
                      import_paths: Annotated[list[RelativePath], Field(max_length=32)] | None = None,
                      root_id: str | None = None,
                      timeout_ms: Annotated[int, Field(strict=True, ge=0, le=3000)] = 1000,
                      lifetime_seconds: Annotated[int, Field(strict=True, ge=1, le=3600)] = 900) -> dict:
        """在無網路 Windows AppContainer 執行 PowerShell；work:\\ 是獨立副本。只匯入明列檔案。

        timeout_ms 是初次等待時間，非命令壽命。返回 session_id 後用 read_process_output 取得輸出，
        interact_with_process 傳入 stdin，完成後明確 export_session_file 與 close_session。
        不繼承主機金鑰、使用者 profile 或工具環境。不能直接存取主機共享根。
        """
        return self.sessions.start(command, import_paths or [], root_id, timeout_ms, lifetime_seconds)

    def read_process_output(self, session_id: SessionId,
                            offset: Annotated[int, Field(strict=True, ge=0)] = 0,
                            length: Annotated[int, Field(strict=True, ge=1, le=32000)] = 24000,
                            wait_ms: Annotated[int, Field(strict=True, ge=0, le=3000)] = 0) -> dict:
        """以絕對字元 offset 讀取有界 session 輸出；next_offset 接續，dropped_chars 明示溢出。"""
        return self.sessions.read(session_id, offset, length, wait_ms)

    def interact_with_process(self, session_id: SessionId,
                              input: Annotated[str, Field(strict=True, max_length=4096)] = '',
                              close_stdin: bool = False) -> dict:
        """送出明確文字到受管理程序的 stdin；需要換行時自行包含換行。非桌面或 TTY 操作。"""
        return self.sessions.interact(session_id, input, close_stdin)

    def list_sessions(self) -> dict:
        """只列本次服務啟動的 sessions；不列舉主機其他程序或命令列。"""
        return self.sessions.list()

    def force_terminate(self, session_id: SessionId) -> dict:
        """終止指定 session 及其 Job 子程序，保留輸出與沙箱檔案供匯出。"""
        return self.sessions.terminate(session_id)

    def close_session(self, session_id: SessionId) -> dict:
        """停止 session、移除其沙箱 profile 並釋放名額；關閉前先匯出需要的檔案。"""
        return self.sessions.close_session(session_id)

    def export_session_file(self, session_id: SessionId, path: RelativePath,
                            destination: RelativePath, expected_sha256: Sha256 | None = None,
                            root_id: str | None = None) -> dict:
        """session 完成後，明確匯出單一 UTF-8 文字檔到共享根；覆寫需原檔 SHA-256。"""
        return self.sessions.export(session_id, path, destination, expected_sha256, root_id)

    def close(self) -> None:
        # Revoke file authority even if subsequent session cleanup fails.
        self.files.close()
        self.sessions.close()
