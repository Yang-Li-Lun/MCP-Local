"""受本次 MCP 服務管理的 AppContainer sessions；無任意 PID 控制。"""
from __future__ import annotations

import codecs
from dataclasses import dataclass, field
import os
from pathlib import Path
import threading
import time
import uuid

from access_mode import require_full_control
from control_files import MAX_WRITE_BYTES
from local_files_mcp import FileReader
from sandbox_windows import Sandbox

MAX_SESSIONS = 16
MAX_ACTIVE = 4
MAX_OUTPUT = 1024 * 1024
MAX_INPUT_BYTES = 8 * 1024 * 1024
MAX_DISK_BYTES = 64 * 1024 * 1024
SESSION_TTL = 900


@dataclass
class Session:
    sandbox: Sandbox
    identifier: str = field(default_factory=lambda: uuid.uuid4().hex)
    started: float = field(default_factory=time.monotonic)
    finished: float | None = None
    output: str = ''
    dropped: int = 0
    exit_code: int | None = None
    reason: str | None = None
    lock: threading.RLock = field(default_factory=threading.RLock)
    io_lock: threading.RLock = field(default_factory=threading.RLock)
    done: threading.Event = field(default_factory=threading.Event)
    closed: bool = False
    backend: str = 'windows_appcontainer'
    host_files_access: str = 'explicit_import_export_only'
    storage_root: Path | None = None

    def append(self, text: str) -> None:
        with self.lock:
            self.output += text
            if len(self.output) > MAX_OUTPUT:
                count = len(self.output) - MAX_OUTPUT
                self.output = self.output[count:]
                self.dropped += count

    def view(self, offset: int = 0, length: int = 24000) -> dict:
        with self.lock:
            actual = min(max(offset, self.dropped), self.dropped + len(self.output))
            content = self.output[actual - self.dropped:actual - self.dropped + length]
            result = dict(session_id=self.identifier, pid=self.sandbox.pid,
                        completed=self.done.is_set(), exit_code=self.exit_code,
                        reason=self.reason, output=content, offset=actual,
                        next_offset=actual + len(content), dropped_chars=self.dropped,
                        truncated=offset < self.dropped, sandbox=self.backend,
                        host_files_access=self.host_files_access)
            if self.backend == 'windows_sandbox_vm':
                result.update(vm_id=self.sandbox.vm_id,
                              vm_stopped=self.sandbox.finished.is_set() and self.sandbox.vm_id is None
                              and self.sandbox.error != 'VM_STOP_UNCONFIRMED')
            return result


def check_disk(profile: Path) -> None:
    pending, count, total = [profile], 0, 0
    while pending:
        folder = pending.pop()
        with os.scandir(folder) as entries:
            for entry in entries:
                count += 1
                info = entry.stat(follow_symlinks=False)
                if count > 4000 or getattr(info, 'st_file_attributes', 0) & 0x400:
                    raise ValueError('sandbox_storage_limit')
                if entry.is_dir(follow_symlinks=False):
                    pending.append(Path(entry.path))
                else:
                    total += info.st_size
                if total > MAX_DISK_BYTES:
                    raise ValueError('sandbox_storage_limit')


class Sessions:
    def __init__(self, files, mode: str):
        self.files, self.mode = files, mode
        self.sessions: dict[str, Session] = {}
        self.lock = threading.RLock()
        self.closed = False

    def require_mode(self) -> None:
        require_full_control(self.mode)

    def get(self, identifier: str) -> Session:
        self.require_mode()
        with self.lock:
            if self.closed or identifier not in self.sessions:
                raise ValueError('SESSION_UNKNOWN：未知或已關閉的 session。')
            return self.sessions[identifier]

    def start(self, command: str, paths: list[str], root_id: str | None,
              timeout_ms: int, lifetime_seconds: int) -> dict:
        self.require_mode()
        if not command.strip() or '\0' in command or len(command) > 8192:
            raise ValueError('COMMAND_LIMIT：命令必須為 1 至 8192 字元。')
        reader = self.files.reader(root_id)
        with self.lock:
            if self.closed:
                raise ValueError('SESSION_CLOSED：服務已關閉。')
            for key, item in list(self.sessions.items()):
                if item.finished and time.monotonic() - item.finished > SESSION_TTL:
                    self.close_session(key)
            if len(self.sessions) >= MAX_SESSIONS or sum(not s.done.is_set() for s in self.sessions.values()) >= MAX_ACTIVE:
                raise ValueError('SESSION_LIMIT：session 名額已滿，請先關閉已完成的 session。')
            if len(paths) > 32 or len(set(paths)) != len(paths):
                raise ValueError('IMPORT_LIMIT：最多明確匯入 32 個不同檔案。')
            inputs, total = [], 0
            # Complete every host-side check before creating a process or profile.
            for path in paths:
                source = reader.checked(path)
                raw = self.files.read_bytes(reader, source)
                total += len(raw)
                if total > MAX_INPUT_BYTES:
                    raise ValueError('IMPORT_LIMIT：匯入總量超過 8 MiB。')
                inputs.append((reader.relative(source), raw))
            sandbox = Sandbox()
            try:
                for path, raw in inputs:
                    destination = sandbox.workspace / path
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(raw)
                sandbox.start(command)
            except BaseException:
                sandbox.close()
                raise
            session = Session(sandbox, storage_root=sandbox.profile.parent)
            self.sessions[session.identifier] = session
            drain = threading.Thread(target=self._drain, args=(session,), daemon=True)
            monitor = threading.Thread(target=self._monitor, args=(session, drain, lifetime_seconds), daemon=True)
            drain.start()
            monitor.start()
        session.done.wait(timeout_ms / 1000)
        return session.view()

    def _drain(self, session: Session) -> None:
        decoder = codecs.getincrementaldecoder('utf-8')('replace')
        try:
            while data := session.sandbox.stdout.read(8192):
                session.append(decoder.decode(data))
            session.append(decoder.decode(b'', final=True))
        except (OSError, ValueError):
            if not session.closed:
                session.reason = 'output_stream_closed'

    def _monitor(self, session: Session, drain: threading.Thread, lifetime: int) -> None:
        last_disk_check = 0
        try:
            while True:
                with session.io_lock:
                    if session.closed:
                        return
                    code = session.sandbox.poll()
                    if code is not None:
                        session.exit_code = code
                        # Descendants cannot survive the top-level command.
                        session.sandbox.stop()
                        break
                    now = time.monotonic()
                    if now - session.started >= lifetime:
                        session.reason = 'lifetime_limit'
                        session.sandbox.stop()
                    elif session.storage_root is not None and now - last_disk_check >= 1:
                        last_disk_check = now
                        try:
                            check_disk(session.storage_root)
                        except (ValueError, OSError):
                            session.reason = 'storage_limit_or_unreadable'
                            session.sandbox.stop()
                time.sleep(.1)
            drain.join(timeout=3)
        except (OSError, ValueError):
            session.reason = 'session_monitor_failed'
            with session.io_lock:
                if not session.closed:
                    session.sandbox.stop()
        finally:
            session.finished = time.monotonic()
            session.done.set()

    def read(self, identifier: str, offset: int, length: int, wait_ms: int) -> dict:
        session = self.get(identifier)
        session.done.wait(wait_ms / 1000)
        return session.view(offset, length)

    def interact(self, identifier: str, text: str, close_stdin: bool) -> dict:
        session = self.get(identifier)
        data = text.encode('utf-8')
        if len(data) > 4096:
            raise ValueError('STDIN_LIMIT：單次輸入最多 4096 bytes。')
        with session.io_lock:
            if session.closed or session.done.is_set() or session.sandbox.stdin.closed:
                raise ValueError('SESSION_FINISHED：此 session 已無法接收輸入。')
            # A bounded writer thread prevents a child that never reads stdin from
            # occupying an MCP worker forever. Timeout terminates the whole Job.
            failure = []
            def write():
                try:
                    session.sandbox.stdin.write(data)
                    if close_stdin:
                        session.sandbox.stdin.close()
                except (OSError, ValueError) as exc:
                    failure.append(exc)
            writer = threading.Thread(target=write, daemon=True)
            writer.start()
            writer.join(timeout=2)
            if writer.is_alive():
                session.reason = 'stdin_timeout'
                session.sandbox.stop()
                raise ValueError('STDIN_TIMEOUT：程序未讀取輸入，已停止 session。')
            if failure:
                raise ValueError('STDIN_CLOSED：程序已關閉輸入。')
        return {'session_id': identifier, 'bytes_sent': len(data), 'stdin_closed': close_stdin}

    def terminate(self, identifier: str) -> dict:
        session = self.get(identifier)
        with session.io_lock:
            if session.closed:
                raise ValueError('SESSION_UNKNOWN：session 已關閉。')
            session.reason = 'terminated_by_client'
            session.sandbox.stop()
        session.done.wait(5)
        return session.view()

    def list(self) -> dict:
        self.require_mode()
        with self.lock:
            if self.closed:
                raise ValueError('SESSION_CLOSED：服務已關閉。')
            return {'sessions': [{k: v for k, v in s.view(length=0).items() if k != 'output'}
                                 for s in self.sessions.values()], 'max_sessions': MAX_SESSIONS,
                    'max_active': MAX_ACTIVE}

    def export(self, identifier: str, path: str, destination: str, expected_sha256: str | None,
               root_id: str | None) -> dict:
        session = self.get(identifier)
        with session.io_lock:
            if session.closed or not session.done.is_set():
                raise ValueError('SESSION_RUNNING：停止 session 後才能匯出，避免沙箱寫入競態。')
            session.sandbox.stop()
            sandbox_reader = FileReader(session.sandbox.workspace)
            source = sandbox_reader.checked(path)
            raw = self.files.read_bytes(sandbox_reader, source)
            # Text-only export uses the same host write guard and optimistic hash.
            content = raw.decode('utf-8')
            result = self.files.write(destination, content,
                                      'rewrite' if expected_sha256 else 'create', expected_sha256, root_id)
            return {**result, 'session_id': identifier}

    def close_session(self, identifier: str) -> dict:
        session = self.get(identifier)
        with session.io_lock:
            session.sandbox.stop()
            session.closed = True
        session.done.wait(5)
        with session.io_lock:
            session.sandbox.close()
        with self.lock:
            del self.sessions[identifier]
        return {'session_id': identifier, 'closed': True,
                'profile_removed': session.backend == 'windows_appcontainer'}

    def close(self) -> None:
        with self.lock:
            errors = []
            for identifier in list(self.sessions):
                try:
                    self.close_session(identifier)
                except Exception as exc:
                    errors.append(exc)
            self.closed = True
            if errors:
                raise ValueError('SESSION_CLEANUP_FAILED：部分沙箱未完成清理。') from errors[0]
