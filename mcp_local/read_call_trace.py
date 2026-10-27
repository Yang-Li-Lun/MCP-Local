"""明確啟用、限時限量的讀取診斷；只記錄固定結構摘要，不記錄內容。"""
import hashlib
import hmac
import json
import os
from pathlib import Path
import secrets
import stat
import sys
import time
from datetime import datetime, timezone
from typing import Any
from io import TextIOWrapper

import anyio
from anyio.abc import ObjectReceiveStream
from mcp import types
from mcp.server.stdio import stdio_server

PROJECT = Path(__file__).resolve().parents[1]
MAX_BYTES = 256 * 1024
MAX_CALLS = 24
MAX_SECONDS = 600
CONTENT_TYPES = frozenset(('text', 'image', 'audio', 'resource', 'resource_link'))


def consume_request(request: Path) -> Path:
    """消耗本機操作者明確建立的單次暫存請求；不讀取正式設定。"""
    if request != PROJECT / '.test-tmp/read-host-trace.request':
        raise ValueError('TRACE_REQUEST：診斷請求位置無效。')
    for node in (request, *request.parents):
        info = node.lstat()
        if node.is_symlink() or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError('TRACE_REQUEST：拒絕連結。')
    info = request.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_size > 80:
        raise ValueError('TRACE_REQUEST：診斷請求屬性無效。')
    with request.open('rb') as handle:
        opened = os.fstat(handle.fileno())
        if (info.st_dev, info.st_ino) != (opened.st_dev, opened.st_ino):
            raise ValueError('TRACE_REQUEST：診斷請求已變更。')
        name = handle.read(81).decode('ascii').strip()
    prefix = 'read-host-trace-'
    suffix = name[len(prefix):]
    if not name.startswith(prefix) or len(suffix) != 32 or any(c not in '0123456789abcdef' for c in suffix):
        raise ValueError('TRACE_REQUEST：診斷目錄名稱無效。')
    current = request.lstat()
    if (current.st_dev, current.st_ino, current.st_size, current.st_mtime_ns) != (
            info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns):
        raise ValueError('TRACE_REQUEST：診斷請求已變更。')
    request.unlink()
    return request.parent / name


def value_type(value: Any) -> str:
    for cls, name in ((type(None), 'null'), (bool, 'boolean'), (dict, 'object'),
                      (list, 'array'), (str, 'string'), (int, 'number'), (float, 'number')):
        if isinstance(value, cls):
            return name
    return 'other'


def payload_shape(value: Any) -> dict:
    result = {'payload_type': value_type(value)}
    if isinstance(value, dict):
        result.update(payload_empty=not value,
                      has_content='content' in value, has_text='text' in value,
                      has_path='path' in value)
    return result


def result_shape(value: Any) -> dict:
    result = {'result_type': value_type(value)}
    if not isinstance(value, dict):
        return result
    result['result_empty'] = not value
    content = value.get('content')
    result.update(content_present='content' in value, content_type=value_type(content),
                  structured_present='structuredContent' in value,
                  structured_type=value_type(value.get('structuredContent')),
                  isError_present='isError' in value,
                  isError=value.get('isError') if type(value.get('isError')) is bool else 'invalid')
    if isinstance(content, list):
        result['content_count'] = len(content)
        result['content_types'] = [item.get('type') if isinstance(item, dict) and
            item.get('type') in CONTENT_TYPES else 'other' for item in content[:8]]
        if content and isinstance(content[0], dict) and content[0].get('type') == 'text':
            text = content[0].get('text')
            if isinstance(text, str) and len(text) <= 2 * 1024 * 1024:
                try:
                    result.update(payload_shape(json.loads(text)))
                except (ValueError, RecursionError):
                    result['payload_type'] = 'non_json_text'
    return result


class Trace:
    def __init__(self, folder: Path) -> None:
        # Hidden repository-local parent keeps diagnostic files outside all file APIs.
        raw = folder.absolute()
        temporary = PROJECT / '.test-tmp'
        if raw.parent != temporary or not raw.name.startswith('read-host-trace-'):
            raise ValueError('TRACE_DIRECTORY：僅接受專案 .test-tmp 下的專用診斷目錄。')
        for node in (raw, *raw.parents):
            if node.is_symlink() or (node.exists() and getattr(node.lstat(), 'st_file_attributes', 0) & 0x400):
                raise ValueError('TRACE_DIRECTORY：拒絕連結或重新解析點。')
        raw.mkdir(parents=True, exist_ok=True)
        self.instance = secrets.token_hex(16)
        self._key = secrets.token_bytes(32)
        self.started = time.monotonic()
        self.handle = (raw / (self.instance + '.jsonl')).open('x', encoding='utf-8')
        self.used = self.sequence = 0
        self.calls = {}
        self.ambiguous = set()
        self.completed = set()
        self.accepted = 0
        self.stopped = False
        relative = PROJECT.name + '/shared/README.md'
        absolute = str(PROJECT / 'shared' / 'README.md')
        self.paths = {absolute: 'plain_absolute', relative: 'plain_relative'}
        examples = []
        for client in ('chatgpt', 'codex'):
            for kind, tool in (('absolute', 'read_host_file'), ('relative', 'read_host_file'), ('control', 'read_file')):
                nonce = secrets.token_hex(8)
                marker = ''.join('./' if bit == '1' else '.\\' for bit in bin(int(nonce, 16))[2:].zfill(64))
                path = (str(PROJECT / 'shared') + os.sep if kind == 'absolute' else
                        PROJECT.name + '/shared/') + marker + 'README.md'
                label = client + '_' + kind
                self.paths[path] = label
                examples.append(dict(label=label, tool=tool,
                    arguments=dict(path=path, start_line=1, line_count=2)))
        self.record('trace_start', pid=os.getpid(), max_calls=MAX_CALLS, max_seconds=MAX_SECONDS)
        # Operator-generated examples only; never request data or credentials.
        with (raw / (self.instance + '.plan.json')).open('x', encoding='utf-8') as plan:
            json.dump(dict(instance=self.instance, examples=examples), plan, indent=2)

    def record(self, stage: str, **fields: Any) -> None:
        if self.stopped:
            return
        try:
            row = dict(instance=self.instance, sequence=self.sequence, stage=stage,
                       utc=datetime.now(timezone.utc).isoformat(),
                       elapsed_ms=round((time.monotonic() - self.started) * 1000, 3), **fields)
            line = json.dumps(row, ensure_ascii=True, separators=(',', ':')) + '\n'
            if self.used + len(line) > MAX_BYTES:
                self.stopped = True
                return
            self.handle.write(line)
            self.handle.flush()
            self.used += len(line)
            self.sequence += 1
        except Exception:
            # Diagnostic I/O must never change a tool result or transport outcome.
            self.stopped = True

    def request_key(self, identifier: Any) -> str | None:
        if type(identifier) not in (str, int) or len(str(identifier)) > 4096:
            return None
        return hmac.new(self._key, json.dumps(identifier).encode(), hashlib.sha256).hexdigest()[:24]

    def receive(self, message: Any) -> None:
        if self.stopped:
            return
        try:
            root = message.message.root
            if not isinstance(root, types.JSONRPCRequest) or root.method != 'tools/call':
                return
            params = root.params or {}
            name, args = params.get('name'), params.get('arguments') or {}
            if name not in ('read_file', 'read_host_file') or not isinstance(args, dict):
                return
            probe = self.paths.get(args.get('path')) if isinstance(args.get('path'), str) else None
            if (probe is None or type(args.get('line_count', 200)) is not int or args.get('line_count', 200) != 2
                    or type(args.get('start_line', 1)) is not int or args.get('start_line', 1) != 1):
                return
            if time.monotonic() - self.started > MAX_SECONDS or self.accepted >= MAX_CALLS:
                self.record('trace_limit')
                self.stopped = True
                return
            key = self.request_key(root.id)
            if key is None or key in self.ambiguous or (key in self.calls and key not in self.completed):
                if key is not None:
                    self.ambiguous.add(key)
                self.record('ambiguous_request_id')
                return
            self.completed.discard(key)
            self.ambiguous.discard(key)
            self.accepted += 1
            self.calls[key] = dict(call=self.accepted, tool=name, probe=probe)
            self.record('request_received', request=key, **self.calls[key])
        except Exception:
            self.stopped = True

    def event(self, identifier: Any, stage: str, **fields: Any) -> None:
        try:
            key = self.request_key(identifier)
            if key in self.calls and key not in self.ambiguous and key not in self.completed:
                self.record(stage, request=key, **self.calls[key], **fields)
        except Exception:
            self.stopped = True

    def shape_event(self, identifier: Any, stage: str, value: Any, *, payload: bool = False) -> None:
        try:
            shape = payload_shape(value) if payload else result_shape(value)
            self.event(identifier, stage, **shape)
        except Exception:
            self.stopped = True

    def close(self) -> None:
        self.record('trace_closed')
        try:
            self.handle.close()
        except Exception:
            pass


class ObservedInput(ObjectReceiveStream):
    def __init__(self, stream: Any, trace: Trace) -> None:
        self.stream, self.trace = stream, trace

    async def receive(self) -> Any:
        message = await self.stream.receive()
        self.trace.receive(message)
        return message

    async def aclose(self) -> None:
        await self.stream.aclose()


class ObservedOutput:
    def __init__(self, stream: Any, trace: Trace) -> None:
        self.stream, self.trace, self.pending = stream, trace, None

    async def write(self, data: str) -> int:
        try:
            self.pending = None
            if not self.trace.stopped and len(self.trace.completed) < len(self.trace.calls):
                value = json.loads(data)
                key = self.trace.request_key(value.get('id'))
                if key in self.trace.calls and key not in self.trace.completed and key not in self.trace.ambiguous:
                    self.pending = (value.get('id'), dict(jsonrpc_result_present='result' in value,
                        jsonrpc_error_present='error' in value, **result_shape(value.get('result'))))
        except Exception:
            self.pending = None
        try:
            result = await self.stream.write(data)
        except BaseException:
            if self.pending:
                self.trace.event(self.pending[0], 'stdout_write_failed')
            raise
        if self.pending:
            self.trace.event(self.pending[0], 'stdout_written',
                             full_write=result == len(data), **self.pending[1])
        return result

    async def flush(self) -> None:
        try:
            result = await self.stream.flush()
        except BaseException:
            if self.pending:
                self.trace.event(self.pending[0], 'stdout_flush_failed')
            raise
        if self.pending:
            self.trace.event(self.pending[0], 'stdout_flushed')
            self.trace.completed.add(self.trace.request_key(self.pending[0]))
        self.pending = None
        return result


def install(server: Any, folder: Path) -> None:
    """Only for a new, explicitly opted-in STDIO process; no tool metadata changes."""
    trace = Trace(folder)
    low = server._mcp_server
    original_handler = low.request_handlers[types.CallToolRequest]

    def request_id():
        try:
            return low.request_context.request_id
        except LookupError:
            return None

    async def handler(request):
        identifier = request_id()
        result = await original_handler(request)
        trace.shape_event(identifier, 'mcp_result', result.root.model_dump(by_alias=True, exclude_none=True))
        return result

    low.request_handlers[types.CallToolRequest] = handler
    for name in ('read_file', 'read_host_file'):
        registered = server._tool_manager.get_tool(name)
        if registered is None:
            continue

        def observe(function):
            async def call(*args, **kwargs):
                identifier = request_id()
                try:
                    result = await function(*args, **kwargs)
                except BaseException:
                    trace.event(identifier, 'tool_raised')
                    raise
                trace.shape_event(identifier, 'tool_result', result, payload=True)
                return result
            return call
        registered.fn = observe(registered.fn)

    async def run_stdio():
        output = anyio.wrap_file(TextIOWrapper(sys.stdout.buffer, encoding='utf-8'))
        try:
            async with stdio_server(stdout=ObservedOutput(output, trace)) as (read, write):
                await low.run(ObservedInput(read, trace), write, low.create_initialization_options())
        finally:
            trace.close()

    server.run_stdio_async = run_stdio
