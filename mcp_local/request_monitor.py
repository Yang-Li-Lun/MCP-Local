"""本機詳細 MCP 偵錯：連線時啟用，原始內容只留在有界記憶體。

觀察 MCP 輸入／輸出 stream，不改工具、協定訊息或 STDIO 編碼。
Windows Local 命名映射只供 GUI 啟動的子程序使用，不建立檔案或網路端點。
名稱為每個 GUI 隨機產生的能力值；沿用目前 Windows 使用者的物件 ACL。
這不是對同一使用者受信任主機命令的隔離邊界。
"""
from __future__ import annotations

from collections import OrderedDict
from contextlib import contextmanager
from datetime import datetime
import json
import math
import os
import secrets
import struct
import time
from typing import Any

from anyio.abc import ObjectReceiveStream, ObjectSendStream
from pydantic import BaseModel

ENV_NAME = 'MCP_LOCAL_REQUEST_MONITOR'
SLOTS = 16
SLOT_BYTES = 1152 * 1024
HEADER_BYTES = 128
MAP_BYTES = HEADER_BYTES + SLOTS * SLOT_BYTES
MAX_RECORDS = 200
MAX_HISTORY_BYTES = 32 * 1024 * 1024
MAX_CAPTURE_BYTES = 1024 * 1024
MAX_NODES = 10000
MAX_DEPTH = 32
MAX_PENDING = 256
DETAIL_PAGE_CHARS = 32 * 1024


def json_text(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(',', ':'))


def capture_message(value: Any, *, incoming: bool = False) -> tuple[Any, list[dict]]:
    """Copy bounded JSON without interpreting/redacting text or unknown fields.

    Notes live outside the captured JSON, so user keys are never overwritten.
    Read Pydantic fields lazily: model_dump would copy an entire large payload.
    """
    budget, nodes = MAX_CAPTURE_BYTES, MAX_NODES
    notes = []

    def note(path, reason, **fields):
        if len(notes) < 32:
            pointer = '/' + '/'.join(str(part).replace('~', '~0').replace('/', '~1')[:120]
                                      for part in path)
            notes.append(dict(path=pointer[:512], reason=reason, **fields))
        elif len(notes) == 32:
            notes.append(dict(path='/', reason='其餘截斷／省略位置不再逐一列出'))

    def string(text, path):
        nonlocal budget
        candidate = text[:max(0, budget - 2)]
        size = len(json_text(candidate).encode('utf-8', 'backslashreplace'))
        while size > budget and candidate:
            candidate = candidate[:max(0, int(len(candidate) * max(0, budget - 2) / size) - 1)]
            size = len(json_text(candidate).encode('utf-8', 'backslashreplace'))
        budget -= size
        if len(candidate) != len(text):
            note(path, '文字超過監控容量，僅保留前段',
                 original_chars=len(text), retained_chars=len(candidate))
        return candidate

    def visit(item, path=(), depth=0):
        nonlocal budget, nodes
        if budget < 16 or nodes <= 0 or depth > MAX_DEPTH:
            note(path, '容量、節點數或巢狀深度上限')
            budget -= 4
            return None
        nodes -= 1
        if isinstance(item, BaseModel):
            def fields():
                for name, field in type(item).model_fields.items():
                    if incoming and name not in item.model_fields_set:
                        continue
                    child = getattr(item, name)
                    if not incoming and child is None:
                        continue
                    yield field.serialization_alias or field.alias or name, child
                for name, child in (item.model_extra or {}).items():
                    if incoming or child is not None:
                        yield name, child
            pairs = fields()
        elif type(item) is dict:
            pairs = iter(item.items())
        else:
            pairs = None
        if pairs is not None:
            result = {}
            budget -= 2
            for key, child in pairs:
                # JSON keys may also be huge or private; preserve them when they fit.
                if not isinstance(key, str) or len(key) > max(0, budget - 16):
                    note(path, '物件欄位名稱超過監控容量，其後欄位省略')
                    break
                key_size = len(json_text(key).encode('utf-8', 'backslashreplace')) + 2
                if budget < key_size + 16 or nodes <= 0:
                    note(path, '物件其後欄位超過監控容量／節點數')
                    break
                budget -= key_size
                # Only protocol-defined binary blocks are summarized. A request
                # argument or arbitrary structuredContent with these keys is text.
                block = len(path) == 3 and path[:2] == ('result', 'content')
                resource = len(path) == 4 and path[:2] == ('result', 'content') and path[3] == 'resource'
                binary = (block and key == 'data' and type(item) is dict and
                          item.get('type') in ('image', 'audio')) or (resource and key == 'blob')
                if not incoming and binary and isinstance(child, str):
                    result[key] = '[二進位內容省略]'
                    budget -= 64
                    padding = 2 if child.endswith('==') else 1 if child.endswith('=') else 0
                    note((*path, key), '二進位內容未展開', encoded_chars=len(child),
                         decoded_bytes_estimate=max(0, len(child) // 4 * 3 - padding))
                else:
                    result[key] = visit(child, (*path, key), depth + 1)
            return result
        if type(item) in (list, tuple):
            result = []
            budget -= 2
            for index, child in enumerate(item):
                if budget < 16 or nodes <= 0:
                    note(path, '陣列其後項目超過監控容量／節點數',
                         original_items=len(item), retained_items=len(result))
                    break
                budget -= 1
                result.append(visit(child, (*path, index), depth + 1))
            return result
        if isinstance(item, str):
            return string(item, path)
        if item is None or type(item) in (bool, int, float):
            if type(item) is float and not math.isfinite(item):
                note(path, '非有限數值無法以標準 JSON 呈現')
                return None
            size = len(json_text(item))
            if size > budget:
                note(path, '數值超過監控容量')
                return None
            budget -= size
            return item
        note(path, '非 JSON 值未展開', type=type(item).__name__[:80])
        return None

    return visit(value), notes


class MemoryChannel:
    """Fixed-size Windows ring; writers never wait for the GUI or another call.

    Header: collection/connection bytes, epoch u64, sequence u64, owner PID u32,
    attachment u32. Each slot contains a bounded JSON observation.
    The owner process handle prevents collection after a GUI crash/PID reuse.
    """

    def __init__(self, token: str | None = None) -> None:
        import ctypes as c
        from ctypes import wintypes as w
        import mmap
        if os.name != 'nt':
            raise OSError('Windows monitoring only')
        self.owner = token is None
        self.token = token or secrets.token_hex(32)
        if len(self.token) != 64 or any(char not in '0123456789abcdef' for char in self.token):
            raise ValueError('Invalid monitor capability')
        self.memory = self.mutex = self.process = None
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        for name, restype, argtypes in (
            ('CreateMutexW', w.HANDLE, (c.c_void_p, w.BOOL, w.LPCWSTR)),
            ('WaitForSingleObject', w.DWORD, (w.HANDLE, w.DWORD)),
            ('ReleaseMutex', w.BOOL, (w.HANDLE,)),
            ('CloseHandle', w.BOOL, (w.HANDLE,)),
            ('OpenProcess', w.HANDLE, (w.DWORD, w.BOOL, w.DWORD)),
        ):
            function = getattr(self.kernel, name)
            function.restype, function.argtypes = restype, argtypes
        name = 'Local\\MCP_RequestMonitor_v2_' + self.token
        try:
            self.mutex = self.kernel.CreateMutexW(None, False, name + '_lock')
            if not self.mutex:
                raise OSError('Monitor lock unavailable')
            self.memory = mmap.mmap(-1, MAP_BYTES, tagname=name)
            if self.owner:
                self.memory[:] = bytes(MAP_BYTES)
                struct.pack_into('<Q', self.memory, 8, 1)
                struct.pack_into('<I', self.memory, 24, os.getpid())
            else:
                with self.locked() as acquired:
                    if not acquired:
                        raise OSError('Monitor busy')
                    pid = struct.unpack_from('<I', self.memory, 24)[0]
                    self.process = self.kernel.OpenProcess(0x100000, False, pid)
                    if not self.process:
                        raise OSError('Monitor owner unavailable')
                    count = struct.unpack_from('<I', self.memory, 28)[0]
                    struct.pack_into('<I', self.memory, 28, min(count + 1, 2**32 - 1))
        except Exception:
            self.close()
            raise

    @contextmanager
    def locked(self, *, wait_ms: int = 0):
        state = self.kernel.WaitForSingleObject(self.mutex, wait_ms) if self.mutex else -1
        acquired = state == 0
        try:
            if state == 0x80:  # An abandoned writer may have left a partial slot.
                self.memory[0] = 0
            yield acquired
        finally:
            if state in (0, 0x80):
                self.kernel.ReleaseMutex(self.mutex)

    def epoch(self) -> int:
        if self.memory is None or not self.memory[0]:
            return 0
        if not self.owner and self.kernel.WaitForSingleObject(self.process, 0) != 0x102:
            return 0
        return struct.unpack_from('<Q', self.memory, 8)[0]

    def reset(self, enabled: bool) -> None:
        """Owner-only epoch change: stop/clear revoke in-flight completions too."""
        if not self.owner:
            raise ValueError('Only GUI may control collection')
        self.memory[0] = 0  # Revoke immediately, even if a writer is busy.
        with self.locked(wait_ms=50) as acquired:
            if not acquired:
                raise OSError('Monitor busy; collection disabled')
            epoch = struct.unpack_from('<Q', self.memory, 8)[0] + 1
            struct.pack_into('<Q', self.memory, 8, epoch)
            struct.pack_into('<Q', self.memory, 16, 0)
            self.memory[HEADER_BYTES:] = bytes(MAP_BYTES - HEADER_BYTES)
            self.memory[0] = int(enabled)

    def disconnect(self) -> None:
        """Owner-only revocation; safe for the connection worker, without GUI I/O."""
        if self.owner and self.memory is not None:
            self.memory[0] = self.memory[1] = 0

    def publish(self, epoch: int, event: dict) -> None:
        if not epoch or self.epoch() != epoch:
            return
        data = json_text(event).encode('utf-8', 'backslashreplace')
        if len(data) > SLOT_BYTES - 4:
            event = {**event}
            for field in ('request', 'response'):
                if field in event:
                    event[field] = None
                    event[field + '_notes'] = [{'path': '/', 'reason': '事件超過記憶體佇列容量，內容省略'}]
            data = json_text(event).encode('utf-8', 'backslashreplace')
        if len(data) > SLOT_BYTES - 4:
            return
        with self.locked() as acquired:
            if not acquired or self.epoch() != epoch:
                return
            sequence = struct.unpack_from('<Q', self.memory, 16)[0]
            offset = HEADER_BYTES + (sequence % SLOTS) * SLOT_BYTES
            # Clear previous bytes so shorter events do not retain old payloads.
            self.memory[offset:offset + SLOT_BYTES] = bytes(SLOT_BYTES)
            self.memory[offset + 4:offset + 4 + len(data)] = data
            struct.pack_into('<I', self.memory, offset, len(data))
            struct.pack_into('<Q', self.memory, 16, sequence + 1)

    def drain(self, cursor: int) -> tuple[list[dict], int, int, int]:
        with self.locked() as acquired:
            if not acquired:
                return [], cursor, 0, 0
            end = struct.unpack_from('<Q', self.memory, 16)[0]
            attached = struct.unpack_from('<I', self.memory, 28)[0]
            start = max(cursor, end - SLOTS)
            dropped = max(0, start - cursor)
            chunks = []
            # Bound work per GUI tick and release the mutex before JSON decoding.
            stop = min(end, start + 4)
            for sequence in range(start, stop):
                offset = HEADER_BYTES + (sequence % SLOTS) * SLOT_BYTES
                size = struct.unpack_from('<I', self.memory, offset)[0]
                if 0 < size <= SLOT_BYTES - 4:
                    chunks.append(self.memory[offset + 4:offset + 4 + size])
                self.memory[offset:offset + SLOT_BYTES] = bytes(SLOT_BYTES)
        return [json.loads(data) for data in chunks], stop, dropped, attached

    def close(self) -> None:
        if self.memory is not None:
            if self.owner:
                self.disconnect()
                self.memory[HEADER_BYTES:] = bytes(MAP_BYTES - HEADER_BYTES)
            self.memory.close()
            self.memory = None
        for name in ('process', 'mutex'):
            handle = getattr(self, name)
            if handle:
                self.kernel.CloseHandle(handle)
                setattr(self, name, None)


class Monitor:
    """GUI-owned history; the connection worker only revokes channel flags."""

    def __init__(self) -> None:
        self.channel = None
        self.enabled = self.connected = self.failed = False
        self.cursor = self.dropped = self.attached = self.used = self.revision = 0
        self.lost_events = self.evicted_records = 0
        self.records: OrderedDict[str, dict] = OrderedDict()

    def environment(self) -> dict[str, str]:
        return {ENV_NAME: self.channel.token} if self.channel and not self.failed else {}

    def _finish_pending(self, status: str) -> None:
        for row in self.records.values():
            if row['status'] == 'running':
                row['status'] = status
                self.revision += 1
                row['_revision'] = self.revision

    def begin_connection(self) -> None:
        """Called on the GUI thread, once for each new Connection.start()."""
        self.poll()
        self._finish_pending('disconnected')
        self.enabled = self.connected = False
        if self.channel:
            self.channel.close()
            self.channel = None
        try:
            self.channel = MemoryChannel()
            self.channel.memory[1] = 1
            self.channel.reset(True)
            self.cursor = self.attached = 0
            self.enabled = self.connected = True
            self.failed = False
        except Exception:
            self.failed = True

    def disconnect(self) -> None:
        if self.channel:
            self.channel.disconnect()

    def set_enabled(self, enabled: bool) -> None:
        self.poll()
        if not self.connected or self.failed:
            return
        try:
            self.channel.reset(enabled)
            self.cursor = 0
            self.enabled = enabled
            self._finish_pending('paused')
        except Exception:
            self.disconnect()
            self.enabled, self.failed = False, True

    def clear(self) -> None:
        self.poll()
        if self.channel:
            try:
                self.channel.reset(self.enabled and self.connected and not self.failed)
                self.cursor = 0
            except Exception:
                self.disconnect()
                self.enabled, self.failed = False, True
        self.records.clear()
        self.used = self.dropped = 0
        self.lost_events = self.evicted_records = 0

    def poll(self) -> None:
        if self.failed or self.channel is None:
            return
        try:
            events, self.cursor, lost, attached = self.channel.drain(self.cursor)
            self.attached = max(self.attached, attached)
            self.dropped += lost
            self.lost_events += lost
            for event in events:
                key = event['id']
                old = self.records.get(key)
                row = {**(old or {}), **event}
                if old:
                    self.used -= old['_bytes']
                self.revision += 1
                row['_revision'] = self.revision
                row['_bytes'] = len(json_text(row).encode('utf-8', 'backslashreplace'))
                self.records[key] = row
                self.used += row['_bytes']
                while len(self.records) > MAX_RECORDS or self.used > MAX_HISTORY_BYTES:
                    _, removed = self.records.popitem(last=False)
                    self.used -= removed['_bytes']
                    self.dropped += 1
                    self.evicted_records += 1
            if not self.channel.memory[1]:
                self.enabled = self.connected = False
                self._finish_pending('disconnected')
            elif self.enabled and not self.channel.epoch():
                self.enabled = False
                self.failed = True
                self._finish_pending('incomplete')
        except Exception:
            self.disconnect()
            self.enabled, self.failed = False, True

    def close(self) -> None:
        self.enabled = self.connected = False
        self.records.clear()
        self.used = 0
        if self.channel:
            self.channel.close()
            self.channel = None


class Observer:
    """Correlate parsed wire messages per MCP process, without touching tools."""

    def __init__(self, channel: MemoryChannel) -> None:
        self.channel = channel
        self.generation = 0
        self.pending: OrderedDict[tuple, dict] = OrderedDict()

    def active(self) -> int:
        epoch = self.channel.epoch()
        if epoch != self.generation:
            self.pending.clear()
            self.generation = epoch
        return epoch

    @staticmethod
    def request_key(identifier: Any) -> tuple | None:
        if type(identifier) is int or (type(identifier) is str and len(identifier) <= 4096):
            return type(identifier), identifier
        return None

    def receive(self, message: Any) -> None:
        from mcp import types
        epoch = self.active()
        if not epoch or not hasattr(message, 'message'):
            return
        root = message.message.root
        if not isinstance(root, types.JSONRPCRequest) or root.method != 'tools/call':
            return
        key = self.request_key(root.id)
        params = root.params or {}
        name = params.get('name')
        metadata = dict(id=secrets.token_hex(16),
                        time=datetime.now().astimezone().isoformat(timespec='milliseconds'),
                        tool=json_text(name[:120])[1:-1].encode('utf-8', 'backslashreplace').decode('utf-8')
                        if isinstance(name, str) else '[無效工具名稱]',
                        status='running')
        payload, notes = capture_message(root, incoming=True)
        if key is None:
            metadata['status'] = 'incomplete'
            notes.append(dict(path='/id', reason='請求 ID 過長，無法追蹤回應'))
        elif key in self.pending:
            previous = self.pending[key]
            self.channel.publish(epoch, {**previous['metadata'], 'status': 'ambiguous'})
            metadata['status'] = 'ambiguous'
            previous['duplicates'] += 1
        else:
            if len(self.pending) >= MAX_PENDING:
                _, removed = self.pending.popitem(last=False)
                self.channel.publish(epoch, {**removed['metadata'], 'status': 'incomplete'})
            self.pending[key] = dict(metadata=metadata, started=time.monotonic(), duplicates=1)
        self.channel.publish(epoch, {**metadata, 'request': payload, 'request_notes': notes})

    def send(self, message: Any, transport_error: Exception | None = None) -> None:
        from mcp import types
        epoch = self.active()
        if not epoch:
            return
        root = message.message.root
        if not isinstance(root, (types.JSONRPCResponse, types.JSONRPCError)):
            return
        key = self.request_key(root.id)
        pending = self.pending.get(key)
        if pending is None:
            return
        if pending['duplicates'] > 1 or pending.get('ambiguous'):
            pending['duplicates'] -= 1
            pending['ambiguous'] = True
            if pending['duplicates'] <= 0:
                self.pending.pop(key)
            return
        self.pending.pop(key)
        elapsed = round((time.monotonic() - pending['started']) * 1000, 2)
        if transport_error is not None:
            payload, notes = capture_message({'error': str(transport_error)})
            status = 'transport_error'
            notes.append(dict(path='/', reason='傳輸失敗；此內容是本機例外，不是已送出的回應'))
        else:
            payload, notes = capture_message(root)
            is_error = isinstance(root, types.JSONRPCError) or root.result.get('isError') is True
            status = 'error' if is_error else 'success'
            if isinstance(root, types.JSONRPCError) and root.error.code == 0:
                status = 'cancelled'
        self.channel.publish(epoch, {**pending['metadata'], 'status': status,
                                    'elapsed_ms': elapsed, 'response': payload, 'response_notes': notes})

    def finish(self) -> None:
        epoch = self.active()
        if epoch:
            for pending in self.pending.values():
                self.channel.publish(epoch, {**pending['metadata'], 'status': 'disconnected'})
        self.pending.clear()


def observe(function, *args) -> None:
    """No raw payload/exception logging, and no monitoring exception escapes."""
    try:
        function(*args)
    except Exception:
        pass


class ObservedInput(ObjectReceiveStream):
    def __init__(self, stream: Any, observer: Observer) -> None:
        self.stream, self.observer = stream, observer

    async def receive(self) -> Any:
        message = await self.stream.receive()
        observe(self.observer.receive, message)
        return message

    async def aclose(self) -> None:
        await self.stream.aclose()


class ObservedOutput(ObjectSendStream):
    def __init__(self, stream: Any, observer: Observer) -> None:
        self.stream, self.observer = stream, observer

    async def send(self, message: Any) -> None:
        try:
            await self.stream.send(message)
        except Exception as exc:
            observe(self.observer.send, message, exc)
            raise
        observe(self.observer.send, message)

    async def aclose(self) -> None:
        await self.stream.aclose()


def install(server: Any, channel: MemoryChannel) -> None:
    """Wrap low.run so the existing read_call_trace STDIO wrapper also composes."""
    low = server._mcp_server
    original = low.run
    observer = Observer(channel)

    async def run(read, write, *args, **kwargs):
        try:
            return await original(ObservedInput(read, observer), ObservedOutput(write, observer),
                                  *args, **kwargs)
        finally:
            observe(observer.finish)
            observe(channel.close)

    low.run = run


def install_from_environment(server: Any) -> None:
    token = os.environ.pop(ENV_NAME, None)
    if token:
        channel = None
        try:
            channel = MemoryChannel(token)
            install(server, channel)
        except Exception:
            if channel:
                observe(channel.close)
