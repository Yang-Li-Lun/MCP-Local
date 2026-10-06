"""開發模式：授權 root 與唯讀工具鏈的 Windows Sandbox VM。"""
from pathlib import Path
from typing import Annotated
import os
import stat
import sys
import threading
import time
import xml.etree.ElementTree as ET

from pydantic import Field
from access_mode import DEVELOPER_CONTROL, FULL_CONTROL, normalize_access_mode
from control_files import ControlledFiles, PROJECT
from control_sessions import Sessions, Session, MAX_SESSIONS, SESSION_TTL
from full_control import FullControl, RelativePath, SessionId, Sha256
from local_files_mcp import linked
import security_policy


def validate_mapping_tree(root: Path, max_entries: int = 200000, *, protect_credentials: bool = False) -> None:
    """拒絕指向樹外的 NTFS 別名；樹內硬連結須完整計數（例如 Git 的程式別名）。"""
    pending, visited = [root], 0
    aliases = {}
    builtin_configs = []
    while pending:
        folder = pending.pop()
        with os.scandir(folder) as entries:
            for entry in entries:
                visited += 1
                if visited > max_entries:
                    raise ValueError('VM_MAPPING_LIMIT：映射樹超過安全檢查上限。')
                if protect_credentials:
                    from developer_toolchains import CREDENTIAL_NAMES
                    if entry.name.casefold() in CREDENTIAL_NAMES:
                        if entry.name.casefold() == '.npmrc' and len(builtin_configs) < 16:
                            builtin_configs.append(Path(entry.path))
                        else:
                            raise ValueError('VM_TOOLCHAIN_CREDENTIALS：工具鏈不可含主機憑證或設定。')
                # Windows DirEntry.stat caches FindFirstFile metadata with zero
                # inode/link counts. Query the path itself for actual NTFS identity.
                info = Path(entry.path).lstat()
                if getattr(info, 'st_file_attributes', 0) & 0x400 or stat.S_ISLNK(info.st_mode):
                    raise ValueError('VM_MAPPING_ALIAS：映射樹不可含 junction 或其他重新解析點。')
                if stat.S_ISDIR(info.st_mode):
                    pending.append(Path(entry.path))
                elif not stat.S_ISREG(info.st_mode) or info.st_nlink < 1:
                    raise ValueError('VM_MAPPING_ALIAS：映射樹不可含特殊檔案。')
                elif info.st_nlink > 1:
                    key = (info.st_dev, info.st_ino)
                    expected, count = aliases.get(key, (info.st_nlink, 0))
                    if expected != info.st_nlink:
                        raise ValueError('VM_MAPPING_ALIAS：硬連結在檢查期間變動。')
                    aliases[key] = (expected, count + 1)
    if any(expected != count for expected, count in aliases.values()):
        raise ValueError('VM_MAPPING_ALIAS：映射樹含有指向樹外的硬連結。')
    from developer_toolchains import safe_builtin_npm_config
    if any(not safe_builtin_npm_config(path, root) for path in builtin_configs):
        raise ValueError('VM_TOOLCHAIN_CREDENTIALS：工具鏈不可含主機憑證或設定。')


def normalize_toolchains(value, *, validate_paths: bool = True) -> list[str]:
    if value is None:
        return []
    if not isinstance(value, list) or len(value) > 16:
        raise ValueError('VM_TOOLCHAINS：最多十六個本機唯讀工具鏈目錄。')
    result = []
    for item in value:
        if not isinstance(item, str) or not item or len(item) > 4096 or any(ord(ch) < 32 for ch in item):
            raise ValueError('VM_TOOLCHAINS：工具鏈路徑格式無效。')
        path = Path(item)
        if not path.is_absolute() or path.drive.startswith('\\\\') or '..' in path.parts:
            raise ValueError('VM_TOOLCHAINS：需要本機絕對目錄。')
        if validate_paths and (not path.is_dir() or any(linked(p) for p in (path, *path.parents))):
            raise ValueError('VM_TOOLCHAINS：目錄不存在或含重新解析點。')
        name = str(path.absolute())
        if name.casefold() in {p.casefold() for p in result}:
            raise ValueError('VM_TOOLCHAINS：工具鏈目錄不可重複。')
        result.append(name)
    return result


def backend_status(toolchains=None) -> dict:
    executable = Path(os.environ.get('SystemRoot', 'C:/Windows')) / 'System32/WindowsSandbox.exe'
    present = os.name == 'nt' and executable.is_file()
    cli = Path(os.environ.get('LOCALAPPDATA', '')) / 'Microsoft/WindowsApps/wsb.exe'
    available = present and cli.is_file()
    return {'backend': 'windows_sandbox_vm', 'ready': available,
            'windows_sandbox_present': present,
            'reason': 'READY_TO_START' if available else 'WINDOWS_SANDBOX_CLI_UNAVAILABLE',
            'execution_adapter_implemented': True, 'host_fallback': False,
            'toolchain_count': len(toolchains or []), 'toolchain_source': 'automatic_readonly_detection',
            'builtin_powershell': True, 'max_active_sessions': 1,
            'mapping_validation': 'performed again before command execution'}


def build_vm_mapping_plan(roots: list[Path], toolchains: list[Path]) -> str:
    """產生可檢查的 VM 目錄映射計畫；不啟動 VM，不等於已驗證的執行後端。"""
    if not 1 <= len(roots) <= 8 or len(toolchains) > 16:
        raise ValueError('VM_MAPPING_LIMIT：最多八個授權 root 與十六個唯讀工具鏈目錄。')
    configuration = ET.Element('Configuration')
    for field in ('Networking', 'ClipboardRedirection', 'AudioInput', 'VideoInput', 'PrinterRedirection', 'vGPU'):
        ET.SubElement(configuration, field).text = 'Disable'
    mapped = ET.SubElement(configuration, 'MappedFolders')
    seen = []
    state = security_policy.STATE_DIR.resolve()
    for index, (raw, readonly) in enumerate([(p, False) for p in roots] + [(p, True) for p in toolchains]):
        raw = Path(raw)
        if not raw.is_absolute() or '..' in raw.parts or not raw.is_dir():
            raise ValueError('VM_MAPPING_PATH：映射需要既有本機絕對目錄。')
        if raw.drive.startswith('\\\\') or any(linked(part) for part in [raw, *raw.parents]):
            raise ValueError('VM_MAPPING_PATH：拒絕網路路徑與連結。')
        path = raw.resolve(strict=True)
        if path.is_relative_to(state) or state.is_relative_to(path):
            raise ValueError('VM_MAPPING_PROTECTED：映射不得涵蓋 MCP 設定或憑證。')
        protected = (PROJECT, Path(sys.base_prefix).resolve(),
                     Path(os.environ.get('SystemRoot', 'C:/Windows')).resolve())
        if not readonly and any(path.is_relative_to(item) or item.is_relative_to(path) for item in protected):
            raise ValueError('VM_MAPPING_PROTECTED：可寫映射不得涵蓋 MCP、主機 Python 或 Windows 系統程式。')
        if any(path.is_relative_to(other) or other.is_relative_to(path) for other in seen):
            raise ValueError('VM_MAPPING_OVERLAP：映射不可重疊。')
        if readonly:
            from developer_toolchains import validate_toolchain_boundary
            validate_toolchain_boundary(raw)
        validate_mapping_tree(path, protect_credentials=readonly)
        seen.append(path)
        item = ET.SubElement(mapped, 'MappedFolder')
        ET.SubElement(item, 'HostFolder').text = str(path)
        ET.SubElement(item, 'SandboxFolder').text = f'C:\\MCP\\mapping-{index}'
        ET.SubElement(item, 'ReadOnly').text = 'true' if readonly else 'false'
    return ET.tostring(configuration, encoding='unicode')


class DeveloperSessions(Sessions):
    def __init__(self, files, mode: str, toolchains: list[str]):
        super().__init__(files, mode)
        self.toolchains = toolchains

    def require_mode(self) -> None:
        if normalize_access_mode(self.mode) != DEVELOPER_CONTROL:
            raise ValueError('DEVELOPER_MODE_REQUIRED：需要本機明確授權的開發模式。')

    def start(self, command: str, working_directory: str | None, root_id: str | None,
              timeout_ms: int, lifetime_seconds: int) -> dict:
        self.require_mode()
        if not backend_status(self.toolchains)['ready']:
            raise ValueError('DEVELOPER_BACKEND_UNAVAILABLE：需要可用的 Windows Sandbox CLI；不會改用主機命令。')
        if not command.strip() or '\0' in command or len(command) > 8192:
            raise ValueError('COMMAND_LIMIT：命令必須為 1 至 8192 字元。')
        from developer_vm import VMProcess
        with self.lock:
            if self.closed:
                raise ValueError('SESSION_CLOSED：服務已關閉。')
            for key, item in list(self.sessions.items()):
                if item.finished and time.monotonic() - item.finished > SESSION_TTL:
                    self.close_session(key)
            if len(self.sessions) >= MAX_SESSIONS or any(not item.done.is_set() for item in self.sessions.values()):
                raise ValueError('SESSION_LIMIT：開發 VM 同時只執行一個命令 session。')
            process = VMProcess(self.files, root_id, working_directory,
                                [Path(p) for p in self.toolchains], lifetime_seconds)
            try:
                process.start(command)
            except BaseException:
                process.close()
                raise
            session = Session(process, backend='windows_sandbox_vm', host_files_access='authorized_roots_and_readonly_toolchains')
            self.sessions[session.identifier] = session
            drain = threading.Thread(target=self._drain, args=(session,), daemon=True)
            monitor = threading.Thread(target=self._monitor, args=(session, drain, lifetime_seconds), daemon=True)
            drain.start()
            monitor.start()
        session.done.wait(timeout_ms / 1000)
        return {**session.view(), 'guest_roots': process.guest_roots,
                'guest_toolchains': process.guest_toolchains}

    def list(self) -> dict:
        return {**super().list(), 'max_active': 1}


class DeveloperControl(FullControl):
    def __init__(self, workspace, mode: str = DEVELOPER_CONTROL, toolchains=None):
        if normalize_access_mode(mode) != DEVELOPER_CONTROL:
            raise ValueError('DEVELOPER_MODE_REQUIRED：需要本機明確授權的開發模式。')
        self.files = ControlledFiles(workspace, FULL_CONTROL)
        if toolchains is None:
            from developer_toolchains import discover_toolchains
            toolchains = discover_toolchains([reader.root for reader in workspace.readers.values()])
        self.sessions = DeveloperSessions(self.files, mode, normalize_toolchains(toolchains))

    def start_process(self, command: Annotated[str, Field(strict=True, min_length=1, max_length=8192)],
                      working_directory: RelativePath | None = None, root_id: str | None = None,
                      timeout_ms: Annotated[int, Field(strict=True, ge=0, le=3000)] = 1000,
                      lifetime_seconds: Annotated[int, Field(strict=True, ge=1, le=3600)] = 900) -> dict:
        """在隔離 Windows Sandbox 執行 PowerShell；root 直接讀寫、自動偵測工具鏈唯讀映射。

        只用 guest 系統環境，不繼承主機 profile／憑證；網路與剪貼簿關閉。
        PATH 含安全安裝工具鏈的 root/cmd/bin/usr/bin/mingw64/bin；無額外工具仍可使用內建 PowerShell。
        初次啟動可能需數十秒，用 session_id 讀取輸出；同時只執行一個 VM session。
        映射含向外硬連結／重新解析點、VM 或清理能力不可用時拒絕，沒有主機 fallback。
        """
        self.files.reader(root_id)
        return self.sessions.start(command, working_directory, root_id, timeout_ms, lifetime_seconds)

    def force_terminate(self, session_id: SessionId) -> dict:
        """停止指定 session 的整個 VM，確認停止後回傳；保留專案內的命令產出。"""
        return self.sessions.terminate(session_id)

    def close_session(self, session_id: SessionId) -> dict:
        """停止並確認 VM 消失，移除本 session 的 I/O 暫存目錄；保留專案內產出。"""
        return self.sessions.close_session(session_id)

    def export_session_file(self, session_id: SessionId, path: RelativePath,
                            destination: RelativePath, expected_sha256: Sha256 | None = None,
                            root_id: str | None = None) -> dict:
        """VM 停止後額外複製工作目錄文字檔至 root；命令產出原本已直接寫入映射 root。"""
        return self.sessions.export(session_id, path, destination, expected_sha256, root_id)
