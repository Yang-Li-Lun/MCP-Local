"""獨立安全政策 v1；舊安裝缺檔維持關閉，損毀與未知版本拒絕降級。"""
from __future__ import annotations

import copy
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile

VERSION = 1
MAX_BYTES = 65536
BUILTINS = ('private_key', 'api_key', 'bearer', 'password', 'email')


def defaults() -> dict:
    return dict(version=VERSION, enabled=False, host_authorization=True,
                detector_enabled=True, response_guard=True, strategy='mask',
                session_expiry='terminate', max_lease_minutes=15,
                rules=[dict(id=name, kind='builtin', pattern=name, enabled=True,
                            risk='medium' if name == 'email' else 'high') for name in BUILTINS])


def normalize(value: dict) -> dict:
    if type(value) is not dict or set(value) != set(defaults()) or type(value.get('version')) is not int or value['version'] != VERSION:
        raise ValueError('SECURITY_POLICY_SCHEMA：政策版本或欄位無效；原檔保留。')
    for key in ('enabled', 'host_authorization', 'detector_enabled', 'response_guard'):
        if type(value[key]) is not bool:
            raise ValueError('SECURITY_POLICY_FLAG：旗標須為布林值。')
    if value['strategy'] not in ('mask', 'block') or value['session_expiry'] not in ('terminate', 'finish'):
        raise ValueError('SECURITY_POLICY_STRATEGY：不支援的處理策略。')
    if type(value['max_lease_minutes']) is not int or not 1 <= value['max_lease_minutes'] <= 60:
        raise ValueError('SECURITY_POLICY_DURATION：授權上限為 1 至 60 分鐘。')
    if value['enabled'] and value['response_guard'] and not value['detector_enabled']:
        raise ValueError('SECURITY_POLICY_DEPENDENCY：回傳防護需要啟用偵測器。')
    rules = value['rules']
    if type(rules) is not list or not 1 <= len(rules) <= 32:
        raise ValueError('SECURITY_RULE_LIMIT：需要 1 至 32 筆規則。')
    identifiers = set()
    for rule in rules:
        if type(rule) is not dict or set(rule) != {'id', 'kind', 'pattern', 'enabled', 'risk'}:
            raise ValueError('SECURITY_RULE_SCHEMA：規則欄位無效。')
        if not isinstance(rule['id'], str) or not re.fullmatch(r'[a-z][a-z0-9_]{0,39}', rule['id']) or rule['id'] in identifiers:
            raise ValueError('SECURITY_RULE_ID：規則識別碼無效或重複。')
        identifiers.add(rule['id'])
        if type(rule['enabled']) is not bool or rule['risk'] not in ('low', 'medium', 'high'):
            raise ValueError('SECURITY_RULE_RISK：風險或啟用值無效。')
        if rule['kind'] == 'builtin':
            if rule['pattern'] not in BUILTINS:
                raise ValueError('SECURITY_RULE_BUILTIN：未知內建規則。')
        elif rule['kind'] == 'literal':
            if not isinstance(rule['pattern'], str) or not 3 <= len(rule['pattern']) <= 128 or '\0' in rule['pattern']:
                raise ValueError('SECURITY_RULE_LITERAL：固定文字長度為 3 至 128 字元。')
        else:
            raise ValueError('SECURITY_RULE_KIND：只支援內建或固定文字規則。')
    return copy.deepcopy(value)


def encoded(value: dict) -> bytes:
    return json.dumps(value, ensure_ascii=True, sort_keys=True, separators=(',', ':')).encode()


def revision(value: dict) -> str:
    return hashlib.sha256(encoded(value)).hexdigest()


class PolicyStore:
    def __init__(self, directory: Path):
        self.directory = Path(directory)
        self.path = self.directory / 'security-policy.json'

    def read(self) -> tuple[dict, str]:
        try:
            with self.path.open('rb') as stream:
                raw = stream.read(MAX_BYTES + 1)
        except FileNotFoundError:
            return defaults(), ''
        if len(raw) > MAX_BYTES:
            raise ValueError('SECURITY_POLICY_SIZE：政策超出容量。')
        try:
            value = normalize(json.loads(raw))
        except (ValueError, TypeError, KeyError):
            raise ValueError('SECURITY_POLICY_INVALID：政策無法驗證；禁止靜默降級。') from None
        return value, hashlib.sha256(raw).hexdigest()

    def save(self, value: dict, expected: str) -> tuple[dict, str]:
        value = normalize(value)
        self.directory.mkdir(parents=True, exist_ok=True)
        lock = self.path.with_suffix('.lock')
        try:
            descriptor = os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        except FileExistsError:
            raise ValueError('SECURITY_POLICY_BUSY：其他儲存作業未完成。') from None
        temporary = None
        try:
            if self.read()[1] != expected:
                raise ValueError('SECURITY_POLICY_CONFLICT：政策已變更，請重新載入。')
            raw = encoded(value)
            with tempfile.NamedTemporaryFile(dir=self.directory, prefix='security-policy-', suffix='.tmp', delete=False) as stream:
                temporary = Path(stream.name)
                stream.write(raw)
                stream.flush()
                os.fsync(stream.fileno())
            os.replace(temporary, self.path)
            temporary = None
            return value, hashlib.sha256(raw).hexdigest()
        finally:
            os.close(descriptor)
            lock.unlink()
            if temporary is not None:
                temporary.unlink(missing_ok=True)
