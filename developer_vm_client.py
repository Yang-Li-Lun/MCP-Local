"""本機 VM guard 的記憶體內認證通道；不寫入設定或通道 profile。"""
from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import uuid
from multiprocessing.connection import Client

GUARD_ENV = 'MCP_LOCAL_VM_GUARD'


def guard_environment() -> dict[str, str]:
    from host_windows import host_environment
    windows = Path(os.environ['SystemRoot'])
    environment = host_environment(Path(__file__).resolve().parent, windows)
    environment['PATH'] = str(windows / 'System32') + ';' + str(windows)
    environment['PSMODULEPATH'] = str(windows / 'System32/WindowsPowerShell/v1.0/Modules')
    return environment


class GuardOwner:
    """由本機 APP 在 Connection Job 建立之前啟動，關閉時等待 VM 停止證明。"""
    def __init__(self, *, breakaway: bool = False):
        self.descriptor = {'address': '\\\\.\\pipe\\MCP-Developer-' + uuid.uuid4().hex,
                           'authkey': os.urandom(32).hex()}
        self.closed = False
        root = Path(__file__).resolve().parent
        flags = subprocess.CREATE_NO_WINDOW
        if breakaway:
            flags |= subprocess.CREATE_BREAKAWAY_FROM_JOB
        self.process = subprocess.Popen(
            [sys._base_executable, '-I', '-B', str(root / 'developer_vm_guard.py')],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            encoding='utf-8', bufsize=1, cwd=root, creationflags=flags,
            env=guard_environment())
        self.process.stdin.write(json.dumps(self.descriptor) + '\n')
        self.process.stdin.flush()
        ready = []
        reader = threading.Thread(target=lambda: ready.append(self.process.stdout.readline(4096)), daemon=True)
        reader.start()
        reader.join(30)
        if reader.is_alive() or not ready or json.loads(ready[0] or '{}') != {'ready': True}:
            self.close()
            raise ValueError('VM_GUARD_UNAVAILABLE：無法建立獨立 VM 清理程序。')

    def encode(self) -> str:
        return json.dumps(self.descriptor, separators=(',', ':'))

    def close(self) -> None:
        if self.closed:
            return
        if not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            code = self.process.wait(timeout=180)
        except subprocess.TimeoutExpired:
            # Do not kill the only component still responsible for VM teardown.
            raise ValueError('VM_STOP_UNCONFIRMED：VM 清理程序仍在執行；禁止重連。') from None
        rows = self.process.stdout.read(8192).splitlines()
        self.process.stdout.close()
        if code or not rows or json.loads(rows[-1]) != {'stopped': True}:
            raise ValueError('VM_STOP_UNCONFIRMED：未取得 VM 停止證明；禁止重連。')
        self.closed = True


class GuardClient:
    def __init__(self, descriptor: str):
        settings = json.loads(descriptor)
        if not isinstance(settings, dict) or set(settings) != {'address', 'authkey'}:
            raise ValueError('VM_GUARD_CONFIGURATION')
        address, key = settings['address'], bytes.fromhex(settings['authkey'])
        if (len(key) != 32 or not isinstance(address, str)
                or not address.startswith('\\\\.\\pipe\\MCP-Developer-')
                or len(address) != len('\\\\.\\pipe\\MCP-Developer-') + 32):
            raise ValueError('VM_GUARD_CONFIGURATION')
        uuid.UUID(address[-32:])
        self.connection = Client(address, family='AF_PIPE', authkey=key)
        self.lock = threading.Lock()
        self.closed = False

    def request(self, message: dict) -> dict:
        with self.lock:
            if self.closed:
                raise ValueError('VM_GUARD_CLOSED')
            raw = json.dumps(message).encode('utf-8')
            if len(raw) > 128 * 1024:
                raise ValueError('VM_CONFIG_LIMIT')
            self.connection.send_bytes(raw)
            if not self.connection.poll(150):
                raise ValueError('VM_GUARD_TIMEOUT')
            reply = json.loads(self.connection.recv_bytes(8192).decode('utf-8'))
            if not isinstance(reply, dict) or reply.get('ok') is not True:
                raise ValueError('VM_OPERATION_FAILED：VM 啟動或停止尚未確認。')
            return reply

    def close(self) -> None:
        with self.lock:
            if not self.closed:
                self.connection.close()
                self.closed = True
