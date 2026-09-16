"""GUI、CLI 與登入背景程序共用的受管理連線核心。"""
from __future__ import annotations
import csv
import ctypes
import io
import os
import queue
import subprocess
import sys
import threading
import time
import random
from enum import Enum
from collections import deque
from connection_settings import PROJECT, require_migration, build_commands, backup_connection_profile
from integrity import IntegrityError

START_WRAPPER = ('import subprocess,sys; '
                 'permit=sys.stdin.buffer.read(1); '
                 'sys.exit(125) if permit != b"1" else None; '
                 'sys.exit(subprocess.call(sys.argv[1:], creationflags=subprocess.CREATE_NO_WINDOW))')

CONNECTION_MUTEX = 'Local\\MCP_Local_Connection'

class ConnectionErrorKind(str, Enum):
    CONFIG_INVALID = 'CONFIG_INVALID'
    MIGRATION_REQUIRED = 'MIGRATION_REQUIRED'
    KEY_MISSING = 'KEY_MISSING'
    KEY_DECRYPT_FAILED = 'KEY_DECRYPT_FAILED'
    CLIENT_MISSING = 'CLIENT_MISSING'
    CLIENT_INTEGRITY_FAILED = 'CLIENT_INTEGRITY_FAILED'
    NETWORK_UNAVAILABLE = 'NETWORK_UNAVAILABLE'
    AUTH_FAILED = 'AUTH_FAILED'
    TUNNEL_INVALID = 'TUNNEL_INVALID'
    DOCTOR_FAILED = 'DOCTOR_FAILED'
    START_FAILED = 'START_FAILED'
    PROCESS_EXITED = 'PROCESS_EXITED'
    CANCELLED = 'CANCELLED'
    INTERNAL_ERROR = 'INTERNAL_ERROR'
    ALREADY_RUNNING = 'ALREADY_RUNNING'

class RetryPolicy:
    delays = (5, 15, 30, 60, 300)
    retryable = {ConnectionErrorKind.NETWORK_UNAVAILABLE, ConnectionErrorKind.DOCTOR_FAILED,
                 ConnectionErrorKind.START_FAILED, ConnectionErrorKind.PROCESS_EXITED}

    def __init__(self):
        self.attempt = 0

    def reset(self) -> None:
        self.attempt = 0

    def next_delay(self) -> float:
        delay = self.delays[min(self.attempt, len(self.delays) - 1)]
        self.attempt += 1
        return delay * random.uniform(.9, 1.1)


def existing_tunnel() -> bool:
    """只檢查程序名稱，不讀取其他程序的金鑰或命令列。"""
    result = subprocess.run(
        ['tasklist.exe', '/FI', 'IMAGENAME eq tunnel-client.exe', '/FO', 'CSV', '/NH'],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        creationflags=subprocess.CREATE_NO_WINDOW, check=True, timeout=10)
    rows = csv.reader(io.StringIO(result.stdout.decode(errors='replace')))
    return any(row and row[0].casefold() == 'tunnel-client.exe' for row in rows)


class Connection:
    """背景執行連線流程，將狀態送回介面執行緒。"""
    def __init__(self, stage_timeout: float = 60):
        self.events = queue.Queue()
        self.cancel = threading.Event()
        self.lock = threading.Lock()
        self.job = None
        self.thread = None
        self.stage_timeout = stage_timeout
        self.error_kind = None
        self.running_since = None

    def start(self, settings: dict, key: str) -> None:
        if self.thread and self.thread.is_alive():
            raise ValueError('連線流程已執行中。')
        self.cancel.clear()
        self.error_kind = None
        self.running_since = None
        self.thread = threading.Thread(target=self._run, args=(settings, key), daemon=True)
        self.thread.start()

    def stop(self) -> None:
        self.cancel.set()
        self.error_kind = ConnectionErrorKind.CANCELLED
        with self.lock:
            if self.job:
                self.job.stop()

    def _run(self, settings: dict, key: str) -> None:
        from tray_windows import Job, kernel
        mutex = None
        process = None
        try:
            self.error_kind = ConnectionErrorKind.MIGRATION_REQUIRED
            require_migration(settings)
            self.error_kind = ConnectionErrorKind.ALREADY_RUNNING
            mutex = kernel.CreateMutexW(None, False, CONNECTION_MUTEX)
            if not mutex:
                raise RuntimeError('無法建立連線鎖定。')
            if ctypes.get_last_error() == 183:
                raise ValueError('已有連線流程，請先在原視窗停止或按 Ctrl+C。')
            if existing_tunnel():
                raise ValueError('已有 tunnel-client 執行中。請先在原本視窗按 Ctrl+C 停止，再從此介面啟動。')
            self.error_kind = ConnectionErrorKind.CONFIG_INVALID
            commands = build_commands(settings)
            backup_connection_profile(commands)
            self.error_kind = ConnectionErrorKind.START_FAILED
            environment = os.environ.copy()
            environment['CONTROL_PLANE_API_KEY'] = key
            # 先啟動等待訊號的 Python 包裝器，納入 Job 後才允許啟動通道。
            # 避免通道在被納管前就建立子程序。
            wrapper = START_WRAPPER
            with self.lock:
                if self.cancel.is_set():
                    return
                self.job = Job()
            for stage, command in zip(('建立設定', '檢查連線', '通道程序執行中'), commands):
                with self.lock:
                    if self.cancel.is_set():
                        break
                    launch_command = [sys._base_executable, '-c', wrapper, *command]
                    if len(subprocess.list2cmdline(launch_command).encode('utf-16-le')) // 2 >= 32767:
                        raise ValueError('完整啟動命令超過 Windows 容量上限。')
                    process = subprocess.Popen(
                        launch_command,
                        stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                        stderr=subprocess.STDOUT, env=environment,
                        cwd=PROJECT, creationflags=subprocess.CREATE_NO_WINDOW)
                    try:
                        self.job.assign(process)
                    except Exception:
                        process.kill()
                        process.wait()
                        raise
                    if self.cancel.is_set():
                        process.kill()
                        break
                    process.stdin.write(b'1')
                    process.stdin.close()
                self.error_kind = (ConnectionErrorKind.DOCTOR_FAILED if stage == '檢查連線' else
                                   ConnectionErrorKind.PROCESS_EXITED if stage == '通道程序執行中' else
                                   ConnectionErrorKind.START_FAILED)
                if stage == '通道程序執行中':
                    self.running_since = time.monotonic()
                self.events.put(('status', stage))
                output = deque(maxlen=8)
                def drain(stream=process.stdout, buffer=output):
                    try:
                        while chunk := stream.read(1024):
                            buffer.append(chunk)
                    finally:
                        stream.close()
                drainer = threading.Thread(target=drain, daemon=True)
                drainer.start()
                deadline = time.monotonic() + self.stage_timeout
                while process.poll() is None:
                    if self.cancel.wait(.05):
                        break
                    if stage != '通道程序執行中' and time.monotonic() >= deadline:
                        raise RuntimeError(f'{stage}逾時（{self.stage_timeout:g} 秒）。請檢查網路及通道設定後重新啟動。')
                if self.cancel.is_set():
                    break
                code = process.returncode
                drainer.join(timeout=1)
                if self.cancel.is_set():
                    break
                if code:
                    # 只顯示分類，不顯示供應商原始輸出，避免未知認證格式外洩。
                    hint = '請確認金鑰、通道識別碼及網路連線；未取得可確認的認證錯誤分類。'
                    raise RuntimeError(f'{stage}失敗（結束代碼 {code}）。{hint}')
                if stage == '通道程序執行中':
                    raise RuntimeError('通道程序已結束，請重新啟動連線。')
        except Exception as exc:
            if isinstance(exc, IntegrityError):
                self.error_kind = ConnectionErrorKind.CLIENT_INTEGRITY_FAILED
            if not self.cancel.is_set():
                self.events.put(('error', str(exc).replace(key, '[已隱藏金鑰]') if key else str(exc)))
        finally:
            key = ''
            if 'environment' in locals():
                environment.pop('CONTROL_PLANE_API_KEY', None)
            cleanup_failed = False
            def cleanup(action):
                nonlocal cleanup_failed
                try:
                    action()
                except Exception:
                    cleanup_failed = True
            with self.lock:
                if self.job:
                    job, self.job = self.job, None
                    cleanup(job.close)
            if process is not None:
                def reap():
                    if process.poll() is None:
                        process.kill()
                    process.wait(timeout=5)
                cleanup(reap)
                if process.stdin:
                    cleanup(process.stdin.close)
            if 'drainer' in locals():
                cleanup(lambda: drainer.join(timeout=2))
            if process is not None and process.stdout:
                cleanup(process.stdout.close)
            if mutex:
                cleanup(lambda: kernel.CloseHandle(mutex))
            if self.cancel.is_set():
                self.error_kind = ConnectionErrorKind.CANCELLED
            if cleanup_failed:
                self.events.put(('error', 'RESOURCE_CLEANUP_FAILED：部分資源清理失敗。'))
            self.events.put(('done', '清理失敗，請檢查本次程序' if cleanup_failed else '已停止'))
