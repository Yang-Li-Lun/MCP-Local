"""本機簽章共享記憶體，不新增服務或網路端點。

Ed25519 私鑰僅留在桌面 Python；MCP 只取得驗章公鑰與回報 HMAC。
Windows 同帳戶軟體仍屬信任範圍；這不是憑證或程序隔離沙箱。
"""
from __future__ import annotations

from contextlib import contextmanager
import ctypes as c
from ctypes import wintypes as w
import hashlib
import hmac
import json
import mmap
import os
import secrets
import struct
import time

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey, Ed25519PublicKey
from mcp_local.policy import encoded

ENV_NAME = 'MCP_LOCAL_SECURITY_CHANNEL'
SLOT_BYTES = 64 * 1024
SLOTS = 9
MAP_BYTES = SLOT_BYTES * SLOTS
FRESH_SECONDS = 3.0


class LocalChannel:
    def __init__(self, descriptor: str | None = None):
        self.owner = descriptor is None
        self.private = Ed25519PrivateKey.generate() if self.owner else None
        self.sequence = 0
        self.last_sequence = 0
        self.slot = None
        self.identity = secrets.token_hex(16)
        self.memory = self.mutex = None
        if self.owner:
            self.token = secrets.token_hex(32)
            self.report_key = secrets.token_bytes(32)
            self.public = self.private.public_key().public_bytes_raw()
        else:
            try:
                value = json.loads(descriptor)
                self.token = value['token']
                self.report_key = bytes.fromhex(value['report'])
                self.public = bytes.fromhex(value['public'])
                if len(self.token) != 64 or any(ch not in '0123456789abcdef' for ch in self.token) or len(self.report_key) != 32 or len(self.public) != 32:
                    raise ValueError()
            except (ValueError, KeyError, TypeError):
                raise ValueError('SECURITY_CHANNEL_DESCRIPTOR') from None
        self.verifier = Ed25519PublicKey.from_public_bytes(self.public)
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        for name, restype, argtypes in (
            ('CreateMutexW', w.HANDLE, (c.c_void_p, w.BOOL, w.LPCWSTR)),
            ('WaitForSingleObject', w.DWORD, (w.HANDLE, w.DWORD)),
            ('ReleaseMutex', w.BOOL, (w.HANDLE,)), ('CloseHandle', w.BOOL, (w.HANDLE,))):
            function = getattr(self.kernel, name)
            function.restype, function.argtypes = restype, argtypes
        name = 'Local\\MCP_Security_v1_' + self.token
        try:
            self.mutex = self.kernel.CreateMutexW(None, False, name + '_lock')
            if not self.mutex:
                raise OSError('SECURITY_CHANNEL_LOCK')
            self.memory = mmap.mmap(-1, MAP_BYTES, tagname=name)
            if self.owner:
                self.memory[:] = bytes(MAP_BYTES)
            else:
                self.read_control()
                with self.locked():
                    for slot in range(1, SLOTS):
                        row = self._read(slot, False)
                        if row is None or time.monotonic() - row.get('time', 0) > FRESH_SECONDS * 2:
                            self.slot = slot
                            self._write(slot, dict(time=time.monotonic(), identity=self.identity, state='attaching'), False)
                            break
                if self.slot is None:
                    raise ValueError('SECURITY_CHANNEL_CAPACITY')
        except Exception:
            self.close()
            raise

    def descriptor(self) -> str:
        if not self.owner:
            raise ValueError('SECURITY_LOCAL_ONLY')
        return json.dumps(dict(token=self.token, public=self.public.hex(), report=self.report_key.hex()))

    @contextmanager
    def locked(self):
        state = self.kernel.WaitForSingleObject(self.mutex, 200) if self.mutex else -1
        try:
            if state != 0:
                raise ValueError('SECURITY_CHANNEL_UNCONFIRMED')
            yield
        finally:
            if state in (0, 0x80):
                self.kernel.ReleaseMutex(self.mutex)

    def _write(self, slot: int, value: dict, control: bool) -> None:
        data = encoded(value)
        signature = self.private.sign(data) if control else hmac.digest(self.report_key, data, 'sha256')
        blob = signature + data
        if len(blob) > SLOT_BYTES - 4:
            raise ValueError('SECURITY_CHANNEL_LIMIT')
        start = slot * SLOT_BYTES
        self.memory[start:start + SLOT_BYTES] = bytes(SLOT_BYTES)
        self.memory[start + 4:start + 4 + len(blob)] = blob
        struct.pack_into('<I', self.memory, start, len(blob))

    def _read(self, slot: int, control: bool) -> dict | None:
        start = slot * SLOT_BYTES
        size = struct.unpack_from('<I', self.memory, start)[0]
        if size == 0:
            return None
        signature_bytes = 64 if control else 32
        if not signature_bytes < size <= SLOT_BYTES - 4:
            raise ValueError('SECURITY_CHANNEL_FRAME')
        blob = self.memory[start + 4:start + 4 + size]
        signature, data = blob[:signature_bytes], blob[signature_bytes:]
        if control:
            self.verifier.verify(signature, data)
        elif not hmac.compare_digest(signature, hmac.digest(self.report_key, data, 'sha256')):
            raise ValueError('SECURITY_CHANNEL_SIGNATURE')
        value = json.loads(data)
        if type(value) is not dict:
            raise ValueError('SECURITY_CHANNEL_FRAME')
        return value

    def publish(self, policy: dict, revision: str, lease: dict, *, healthy: bool) -> None:
        if not self.owner:
            raise ValueError('SECURITY_LOCAL_ONLY')
        with self.locked():
            self.sequence += 1
            self._write(0, dict(sequence=self.sequence, time=time.monotonic(), policy=policy,
                                revision=revision, lease=lease, healthy=healthy), True)

    def read_control(self) -> dict:
        with self.locked():
            value = self._read(0, True)
        now = time.monotonic()
        if value is None or not 0 <= now - value['time'] <= FRESH_SECONDS or value['sequence'] < self.last_sequence:
            raise ValueError('SECURITY_CHANNEL_STALE')
        self.last_sequence = value['sequence']
        return value

    def report(self, value: dict) -> None:
        if self.owner or self.slot is None:
            raise ValueError('SECURITY_CHANNEL_ROLE')
        with self.locked():
            old = self._read(self.slot, False)
            if old is None or old['identity'] != self.identity:
                raise ValueError('SECURITY_CHANNEL_REPLACED')
            self._write(self.slot, {**value, 'identity': self.identity, 'time': time.monotonic()}, False)

    def reports(self) -> list[dict]:
        if not self.owner:
            raise ValueError('SECURITY_LOCAL_ONLY')
        with self.locked():
            return [value for slot in range(1, SLOTS) if (value := self._read(slot, False)) is not None]

    def close(self) -> None:
        if self.memory is not None:
            if self.owner:
                self.memory[:SLOT_BYTES] = bytes(SLOT_BYTES)
            self.memory.close()
            self.memory = None
        if self.mutex:
            self.kernel.CloseHandle(self.mutex)
            self.mutex = None
