"""固定 metadata 安全事件、SHA-256 鏈與受控清除；不收原文、路徑、命令或例外。"""
from __future__ import annotations

from datetime import datetime, timezone
from contextlib import contextmanager
import hashlib
import json
import os
from pathlib import Path
import threading
import tempfile
import uuid

from mcp_local.policy import encoded

EVENTS = frozenset({'policy_saved', 'authorization_granted', 'authorization_revoked',
                    'authorization_expired', 'channel_failed', 'channel_recovered',
                    'response_masked', 'response_blocked', 'scan_failed', 'command_denied',
                    'session_stopped', 'session_stop_failed', 'synthetic_test', 'audit_cleared',
                    'events_lost'})
MAX_BYTES = 4 * 1024 * 1024


def event(kind: str, *, risk: str = 'low', count: int = 1) -> dict:
    if kind not in EVENTS or risk not in ('low', 'medium', 'high') or type(count) is not int or not 1 <= count <= 1000000:
        raise ValueError('SECURITY_AUDIT_EVENT：事件格式無效。')
    return dict(id=uuid.uuid4().hex, time=datetime.now(timezone.utc).isoformat(),
                kind=kind, risk=risk, count=count)


def validate_event(item: dict) -> dict:
    import re
    required = {'id', 'time', 'kind', 'risk', 'count'}
    if type(item) is not dict or not required <= set(item) or set(item) - required - {'prior_head'}:
        raise ValueError('SECURITY_AUDIT_EVENT')
    event(item['kind'], risk=item['risk'], count=item['count'])
    if not isinstance(item['id'], str) or not re.fullmatch('[a-f0-9]{32}', item['id']):
        raise ValueError('SECURITY_AUDIT_ID')
    if not isinstance(item['time'], str) or len(item['time']) > 40:
        raise ValueError('SECURITY_AUDIT_TIME')
    datetime.fromisoformat(item['time'])
    if 'prior_head' in item and (item['kind'] != 'audit_cleared' or not re.fullmatch('[a-f0-9]{64}', item['prior_head'])):
        raise ValueError('SECURITY_AUDIT_ANCHOR')
    return dict(item)


class SecurityAudit:
    def __init__(self, directory: Path):
        self.path = Path(directory) / 'security-audit.jsonl'
        self.lock = threading.RLock()

    @contextmanager
    def transaction(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        lock = self.path.with_suffix('.lock')
        try:
            descriptor = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            raise ValueError('SECURITY_AUDIT_BUSY：其他事件寫入尚未完成。') from None
        try:
            yield
        finally:
            os.close(descriptor)
            lock.unlink()

    def replace(self, rows: list[dict]) -> None:
        temporary = None
        try:
            with tempfile.NamedTemporaryFile(dir=self.path.parent, prefix='security-audit-', suffix='.tmp', delete=False) as stream:
                temporary = Path(stream.name)
                for row in rows:
                    stream.write(encoded(row) + b'\n')
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            temporary = None
        finally:
            if temporary is not None:
                temporary.unlink(missing_ok=True)

    def read(self) -> list[dict]:
        with self.lock:
            try:
                with self.path.open('rb') as stream:
                    raw = stream.read(MAX_BYTES + 1)
            except FileNotFoundError:
                return []
            if len(raw) > MAX_BYTES:
                raise ValueError('SECURITY_AUDIT_CAPACITY：事件已滿；請匯出後受控清除。')
            previous, rows = '0' * 64, []
            try:
                for line in raw.splitlines():
                    row = json.loads(line)
                    if set(row) != {'event', 'previous', 'hash'}:
                        raise ValueError()
                    validate_event(row['event'])
                    if row['previous'] != previous or row['hash'] != hashlib.sha256(encoded({'event': row['event'], 'previous': previous})).hexdigest():
                        raise ValueError()
                    rows.append(row)
                    previous = row['hash']
            except (ValueError, KeyError, TypeError):
                raise ValueError('SECURITY_AUDIT_INTEGRITY：事件鏈無法驗證；原檔保留。') from None
            return rows

    def append(self, item: dict) -> None:
        item = validate_event(item)
        with self.lock, self.transaction():
            rows = self.read()
            if any(row['event']['id'] == item['id'] for row in rows):
                return
            value = dict(event=item, previous=rows[-1]['hash'] if rows else '0' * 64)
            value['hash'] = hashlib.sha256(encoded(value)).hexdigest()
            raw = encoded(value) + b'\n'
            if sum(len(encoded(row)) + 1 for row in rows) + len(raw) > MAX_BYTES:
                raise ValueError('SECURITY_AUDIT_CAPACITY：請匯出後清除，禁止遺失事件。')
            self.replace([*rows, value])

    def query(self, kind: str = '', risk: str = '', offset: int = 0) -> dict:
        if kind and kind not in EVENTS or risk and risk not in ('low', 'medium', 'high') or type(offset) is not int or offset < 0:
            raise ValueError('SECURITY_AUDIT_FILTER')
        rows = self.read()
        filtered = [row for row in reversed(rows) if (not kind or row['event']['kind'] == kind) and (not risk or row['event']['risk'] == risk)]
        return dict(rows=filtered[offset:offset + 100], total=len(filtered), offset=offset,
                    revision=rows[-1]['hash'] if rows else '0' * 64, integrity='verified')

    def export(self) -> dict:
        rows = self.read()
        return dict(version=1, contains_sensitive_originals=False, chain=rows,
                    revision=rows[-1]['hash'] if rows else '0' * 64,
                    integrity='sha256_chain_not_account_isolation')

    def clear(self, expected: str, confirmation: str) -> None:
        if confirmation != '清除安全事件':
            raise ValueError('SECURITY_AUDIT_CONFIRM：請輸入「清除安全事件」。')
        with self.lock, self.transaction():
            if self.export()['revision'] != expected:
                raise ValueError('SECURITY_AUDIT_CONFLICT：事件已更新，請重新檢視。')
            # Keep a verifiable boundary anchor without retaining deleted event data.
            value = dict(event={**event('audit_cleared', count=max(1, len(self.read()))), 'prior_head': expected}, previous='0' * 64)
            value['hash'] = hashlib.sha256(encoded(value)).hexdigest()
            self.replace([value])
