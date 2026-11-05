"""最後傳輸邊界的完整 JSON 檢查；任何無法檢查的資料均不放行。"""
from __future__ import annotations

from dataclasses import replace
import json
from typing import Any

from anyio.abc import ObjectSendStream
from mcp_local.sensitive_detector import SensitiveDetector, MAX_CHARS

BLOCK_TEXT = 'SECURITY_RESPONSE_BLOCKED：回傳含敏感資訊或無法完整檢查；請在本機安全中心檢視事件。'


class ResponseGuard:
    def __init__(self, policy: dict):
        self.policy = policy
        self.detector = SensitiveDetector(policy)

    def inspect(self, payload: Any) -> dict:
        if not self.policy['enabled'] or not self.policy['response_guard']:
            return dict(action='disabled', payload=payload, findings=[], checked=False)
        findings, nodes, chars = [], 0, 0

        def visit(value, depth=0):
            nonlocal nodes, chars
            nodes += 1
            if nodes > 50000 or depth > 32:
                raise ValueError('SECURITY_SCAN_LIMIT')
            if isinstance(value, str):
                chars += len(value)
                if chars > MAX_CHARS:
                    raise ValueError('SECURITY_SCAN_LIMIT')
                text, found = self.detector.inspect(value)
                findings.extend(found)
                if len(findings) > 10000:
                    raise ValueError('SECURITY_SCAN_LIMIT')
                # FastMCP duplicates structured output in text JSON. Inspect both.
                if value.lstrip().startswith(('{', '[')):
                    try:
                        parsed = json.loads(value)
                    except (ValueError, RecursionError):
                        pass
                    else:
                        return json.dumps(visit(parsed, depth + 1), ensure_ascii=False)
                return text
            if type(value) is list:
                return [visit(item, depth + 1) for item in value]
            if type(value) is dict:
                if value.get('type') in ('image', 'audio', 'resource_link') or 'blob' in value or value.get('encoding') in ('base64', 'hex'):
                    raise ValueError('SECURITY_UNINSPECTABLE')
                result = {}
                for key, item in value.items():
                    safe_key = visit(key, depth + 1)
                    if safe_key in result:
                        raise ValueError('SECURITY_KEY_COLLISION')
                    # Detect assignment context even when key and value are separate.
                    if isinstance(item, (str, int, float)) and not isinstance(item, bool):
                        _, found = self.detector.inspect(f'{key}={item}')
                        contextual = [row for row in found if any(rule['id'] == row['rule'] and rule['kind'] == 'builtin' and rule['pattern'] == 'password' for rule in self.policy['rules'])]
                        if contextual:
                            findings.extend(contextual)
                            result[safe_key] = '[已遮罩]'
                            continue
                    result[safe_key] = visit(item, depth + 1)
                return result
            if value is None or type(value) in (bool, int, float):
                return value
            raise ValueError('SECURITY_UNKNOWN_VALUE')

        try:
            safe = visit(payload)
        except Exception:
            return dict(action='blocked', payload=None, findings=[], checked=False, reason='uninspectable')
        if findings and self.policy['strategy'] == 'block':
            return dict(action='blocked', payload=None, findings=findings, checked=True, reason='sensitive')
        return dict(action='masked' if findings else 'passed', payload=safe, findings=findings, checked=True)


class GuardedOutput(ObjectSendStream):
    def __init__(self, stream, runtime):
        self.stream, self.runtime = stream, runtime

    async def send(self, message) -> None:
        from mcp import types
        root = message.message.root
        # Guard all responses, protocol errors, and server notifications. IDs and
        # protocol fields remain intact; only payloads are inspected/replaced.
        if isinstance(root, types.JSONRPCResponse):
            result = self.runtime.protect(root.result)
            safe = root.model_copy(update={'result': result})
        elif isinstance(root, types.JSONRPCError):
            value = self.runtime.protect(root.error.model_dump(exclude_none=True))
            if value.get('isError'):
                safe = root.model_copy(update={'error': types.ErrorData(code=root.error.code, message=BLOCK_TEXT)})
            else:
                safe = root.model_copy(update={'error': types.ErrorData.model_validate(value)})
        elif isinstance(root, types.JSONRPCNotification):
            value = self.runtime.protect(root.params or {})
            if value.get('isError'):
                return  # Do not emit an uninspectable notification.
            safe = root.model_copy(update={'params': value})
        else:
            safe = root
        await self.stream.send(replace(message, message=types.JSONRPCMessage(safe)))

    async def aclose(self) -> None:
        await self.stream.aclose()


def install(server, runtime) -> None:
    original = server._mcp_server.run

    async def run(read, write, *args, **kwargs):
        runtime.start()
        try:
            return await original(read, GuardedOutput(write, runtime), *args, **kwargs)
        finally:
            runtime.close()

    server._mcp_server.run = run
