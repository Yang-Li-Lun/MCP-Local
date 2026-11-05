"""安全中心協調：本機 UI authority 與 MCP 執行層分離。"""
from __future__ import annotations

from collections import deque
import os
from pathlib import Path
import threading
import time

from mcp_local.host_authorization import HostAuthorization, require_authorized
from mcp_local.local_channel import LocalChannel, ENV_NAME, FRESH_SECONDS
from mcp_local.policy import PolicyStore, defaults, normalize, revision
from mcp_local.response_guard import ResponseGuard, BLOCK_TEXT
from mcp_local.security_audit import SecurityAudit, event
from mcp_local.sensitive_detector import SYNTHETIC


def blocked() -> dict:
    return dict(isError=True, content=[dict(type='text', text=BLOCK_TEXT)])


def finding_risk(result: dict) -> str:
    levels = {'low': 0, 'medium': 1, 'high': 2}
    return max((row['risk'] for row in result['findings']), key=levels.get, default='high')


class SecurityCenter:
    """僅由私有 WPF 管線建立；不在任何 MCP 工具中公開。"""
    def __init__(self, directory: Path):
        self.store = PolicyStore(directory)
        self.audit = SecurityAudit(directory)
        self.authorization = HostAuthorization()
        self.lock = threading.RLock()
        self.policy, self.saved_revision = defaults(), ''
        self.error = None
        self.channel = None
        self.rows = []
        self.seen = set()
        self.seen_lost = {}
        self.recovery = 0
        self.last_load = 0
        self.stop_event = threading.Event()
        try:
            self.policy, self.saved_revision = self.store.read()
            self.audit.read()
            self.channel = LocalChannel()
            self._publish()
        except Exception:
            self.error = 'SECURITY_UNCONFIRMED：政策、事件或本機通訊無法驗證；請診斷並恢復。'
        self.thread = threading.Thread(target=self._loop, name='security-center', daemon=True)
        self.thread.start()

    def _publish(self) -> None:
        if self.channel:
            lease = {**self.authorization.snapshot(), 'recovery': self.recovery,
                     'acks': {row['identity']: [item['id'] for item in row.get('events', []) if item['id'] in self.seen] for row in self.rows}}
            self.channel.publish(self.policy, revision(self.policy), lease, healthy=self.error is None)

    def _loop(self) -> None:
        while not self.stop_event.wait(.25):
            self.tick()

    def tick(self) -> None:
        with self.lock:
            try:
                if time.monotonic() - self.last_load >= 1:
                    policy, saved = self.store.read()
                    if saved != self.saved_revision:
                        raise ValueError('SECURITY_EXTERNAL_CHANGE')
                    self.last_load = time.monotonic()
                if self.channel:
                    self.rows = self.channel.reports()
                    for row in self.rows:
                        for item in row.get('events', []):
                            if item['id'] not in self.seen:
                                self.audit.append(item)
                                self.seen.add(item['id'])
                        lost = row.get('lost_events', 0)
                        if lost > self.seen_lost.get(row['identity'], 0):
                            self.audit.append(event('events_lost', risk='high', count=lost - self.seen_lost.get(row['identity'], 0)))
                            self.seen_lost[row['identity']] = lost
                            self.error = 'SECURITY_AUDIT_LOST：部分安全事件未送達；已記錄缺口，請檢視後恢復。'
                            self.authorization.revoke('audit_lost')
                        if len(self.seen) > 8192:
                            self.seen = {item['id'] for report in self.rows for item in report.get('events', [])}
                    active = [row for row in self.rows if row.get('state') != 'closed' and time.monotonic() - row['time'] <= FRESH_SECONDS]
                    if self.authorization.snapshot()['authorized'] and (not active or any(row.get('state') != 'ready' for row in active)):
                        self.authorization.revoke('channel_failed')
                        self.audit.append(event('authorization_revoked', risk='high'))
                    if self.authorization.deadline and self.authorization.snapshot()['remaining_seconds'] == 0:
                        self.authorization.revoke('expired')
                        self.audit.append(event('authorization_expired'))
                self._publish()
            except Exception:
                self.authorization.revoke('channel_failed')
                self.error = 'SECURITY_UNCONFIRMED：政策、事件或通訊異常，授權已失效；請恢復。'
                try:
                    self._publish()
                except Exception:
                    pass

    def environment(self) -> dict:
        with self.lock:
            if self.error or not self.channel:
                raise ValueError('SECURITY_UNCONFIRMED：安全中心未就緒，不能啟動連線。')
            self.authorization.revoke('connection_started')
            self._publish()
            return {ENV_NAME: self.channel.descriptor()}

    def disconnect(self) -> None:
        with self.lock:
            self.authorization.revoke('connection_stopped')
            try:
                self._publish()
            except Exception:
                self.error = 'SECURITY_CHANNEL_FAILED：連線停止後通訊未確認。'

    def snapshot(self) -> dict:
        with self.lock:
            now = time.monotonic()
            expected = revision(self.policy)
            clients = [dict(identity=row['identity'], age_seconds=round(now - row['time'], 1),
                            state=row.get('state', 'unknown') if now - row['time'] <= FRESH_SECONDS else 'stale',
                            revision=row.get('revision', ''), mode=row.get('mode', ''),
                            sessions=row.get('sessions', []), counters=row.get('counters', {}),
                            lease_generation=row.get('lease_generation', -1),
                            lost_events=row.get('lost_events', 0)) for row in self.rows]
            active = [row for row in clients if row['state'] != 'closed']
            effective = ('error' if self.error else 'no_client' if not active else
                         'confirmed' if all(row['state'] == 'ready' and row['revision'] == expected for row in active) else 'unconfirmed')
            return dict(policy=self.policy, saved_revision=self.saved_revision, policy_revision=expected,
                        migration='existing_policy' if self.saved_revision else 'legacy_defaults_off_not_saved',
                        effective=effective, error=self.error, authorization=self.authorization.snapshot(),
                        clients=clients, channel=dict(available=self.channel is not None, freshness_seconds=FRESH_SECONDS,
                        recovery=self.recovery, transport='local_signed_memory', credential_sandbox=False))

    def save(self, value: dict, expected: str) -> dict:
        with self.lock:
            if self.error:
                raise ValueError('SECURITY_UNCONFIRMED：請先恢復；不覆寫無法驗證的檔案。')
            value = normalize(value)
            self.policy, self.saved_revision = self.store.save(value, expected)
            self.authorization.revoke('policy_changed')
            try:
                self.audit.append(event('policy_saved'))
            except Exception:
                self.error = 'SECURITY_AUDIT_FAILED：政策已儲存，但事件未確認；執行層拒絕降級。'
                self._publish()
                raise
            self._publish()
            return self.snapshot()

    def grant(self, minutes: int) -> dict:
        with self.lock:
            self.tick()
            state = self.snapshot()
            if not self.policy['enabled'] or not self.policy['host_authorization'] or state['effective'] != 'confirmed' or not any(
                    row['mode'] == 'host_control' and row['lease_generation'] == self.authorization.generation
                    for row in state['clients'] if row['state'] == 'ready'):
                raise ValueError('HOST_AUTH_UNCONFIRMED：需要已確認套用政策的主機模式連線。')
            if type(minutes) is not int or not 1 <= minutes <= self.policy['max_lease_minutes']:
                raise ValueError('HOST_AUTH_DURATION：授權時間超過政策上限。')
            self.audit.append(event('authorization_granted', risk='high'))
            self.authorization.grant(minutes, self.policy['max_lease_minutes'])
            self._publish()
            return self.snapshot()

    def revoke(self) -> dict:
        with self.lock:
            self.authorization.revoke()
            self._publish()
            self.audit.append(event('authorization_revoked'))
            return self.snapshot()

    def recover(self) -> dict:
        with self.lock:
            self.authorization.revoke('recovery')
            policy, saved = self.store.read()
            if self.saved_revision and not saved:
                raise ValueError('SECURITY_POLICY_MISSING：原政策遺失；請還原原檔，不能降級為預設關閉。')
            if self.saved_revision and revision(policy) != revision(self.policy):
                raise ValueError('SECURITY_POLICY_CHANGED：外部政策內容已變更；請還原已確認的政策後再恢復。')
            self.audit.read()
            self.policy, self.saved_revision = policy, saved
            if self.channel is None:
                self.channel = LocalChannel()
            self.error = None
            self.recovery += 1
            self.audit.append(event('channel_recovered'))
            self._publish()
            return self.snapshot()

    def preview(self, value: dict, sample: str) -> dict:
        policy = normalize(value)
        if sample not in ('credentials', 'binary', 'ordinary'):
            raise ValueError('SECURITY_SYNTHETIC_SAMPLE')
        payload = ({'text': SYNTHETIC, 'nested': {'password': 'synthetic_only'}} if sample == 'credentials' else
                   {'type': 'image', 'data': 'c3ludGhldGlj'} if sample == 'binary' else {'text': 'Get-ChildItem; python -V; git status; dotnet build'})
        result = ResponseGuard(policy).inspect(payload)
        self.audit.append(event('synthetic_test'))
        return {**result, 'synthetic_only': True, 'draft_policy': True}

    def close(self) -> None:
        self.stop_event.set()
        self.thread.join(timeout=2)
        with self.lock:
            self.authorization.revoke('desktop_closed')
            if self.channel:
                self.channel.close()
                self.channel = None


class SecurityRuntime:
    """MCP 消費者不能核准或延長授權；通訊異常鎖定至本機明確恢復。"""
    def __init__(self, directory: Path, descriptor: str | None = None):
        self.store = PolicyStore(directory)
        self.channel = LocalChannel(descriptor) if descriptor else None
        self.policy, self.disk_revision = (defaults(), '') if self.channel else self.store.read()
        self.protection_seen = self.policy['enabled']
        self.audit = SecurityAudit(directory) if not self.channel else None
        self.lease = {}
        self.confirmed = False
        self.fault = False
        self.fault_recovery = -1
        self.lock = threading.RLock()
        self.events = deque(maxlen=128)
        self.lost_events = 0
        self.counters = dict(passed=0, masked=0, blocked=0, disabled=0, session_masked=0, session_blocked=0)
        self.control = None
        self.mode = 'read_only'
        self.stop_event = threading.Event()
        self.thread = None
        self.stop_results = {}
        self.refresh()

    def fail(self) -> None:
        self.fault = True
        self.confirmed = False
        self.fault_recovery = max(self.fault_recovery, self.lease.get('recovery', 0))

    def record(self, kind: str, risk: str = 'low') -> None:
        item = event(kind, risk=risk)
        with self.lock:
            if self.audit:
                self.audit.append(item)
            if len(self.events) == self.events.maxlen:
                self.lost_events += 1
                self.fail()
                raise ValueError('SECURITY_AUDIT_BACKPRESSURE：事件尚未確認，暫停回傳。')
            self.events.append(item)

    def refresh(self) -> None:
        with self.lock:
            try:
                if self.channel:
                    value = self.channel.read_control()
                    policy = normalize(value['policy'])
                    if revision(policy) != value['revision']:
                        raise ValueError('SECURITY_POLICY_DIGEST')
                    self.lease = value['lease']
                    self.protection_seen |= policy['enabled']
                    acknowledged = set(self.lease.get('acks', {}).get(self.channel.identity, []))
                    self.events = deque((item for item in self.events if item['id'] not in acknowledged), maxlen=128)
                    if not value['healthy']:
                        raise ValueError('SECURITY_AUTHORITY_UNHEALTHY')
                    if self.fault and self.lease.get('recovery', 0) <= self.fault_recovery:
                        self.confirmed = False
                        return
                    self.fault = False
                    self.policy = policy
                    self.protection_seen = policy['enabled']
                    self.confirmed = True
                else:
                    value, current = self.store.read()
                    if self.disk_revision and not current:
                        raise ValueError('SECURITY_POLICY_MISSING')
                    self.policy, self.disk_revision = value, current
                    self.protection_seen |= value['enabled']
                    self.confirmed = not self.fault
            except Exception:
                if not self.fault:
                    try:
                        self.record('channel_failed', 'high')
                    except Exception:
                        pass
                self.fault = True
                self.fault_recovery = max(self.fault_recovery, self.lease.get('recovery', 0))
                self.confirmed = False

    def require_host(self) -> None:
        self.refresh()
        with self.lock:
            try:
                if self.fault and self.protection_seen:
                    raise ValueError('SECURITY_UNCONFIRMED：安全狀態無法確認。')
                require_authorized(self.policy, self.lease, self.confirmed)
            except ValueError:
                self.record('command_denied', 'high')
                raise

    def session_output(self, session) -> tuple[str, str]:
        self.refresh()
        if not self.policy['enabled'] or not self.policy['response_guard']:
            if self.fault and self.protection_seen:
                return BLOCK_TEXT, 'blocked_unconfirmed'
            return session.output, 'disabled'
        if self.fault or session.dropped:
            return BLOCK_TEXT, 'blocked_incomplete'
        if not session.done.is_set():
            return '', 'withheld_until_complete'  # A later chunk can complete a secret.
        result = ResponseGuard(self.policy).inspect(session.output)
        if result['action'] == 'blocked':
            self.counters['session_blocked'] += 1
            self.record('response_blocked', finding_risk(result))
            return BLOCK_TEXT, 'blocked'
        if result['action'] == 'masked':
            self.counters['session_masked'] += 1
            self.record('response_masked', finding_risk(result))
        return result['payload'], result['action']

    def protect(self, value: dict) -> dict:
        self.refresh()
        with self.lock:
            if self.fault and self.protection_seen:
                self.counters['blocked'] += 1
                return blocked()
            result = ResponseGuard(self.policy).inspect(value)
            self.counters[result['action']] += 1
            try:
                if result['action'] in ('masked', 'blocked'):
                    self.record('response_' + result['action'], finding_risk(result))
            except Exception:
                self.fail()
                return blocked()
            output = blocked() if result['action'] == 'blocked' else result['payload']
        try:
            self._report()
        except Exception:
            self.fail()
            return blocked() if self.protection_seen else output
        return output

    def _sessions(self) -> list[dict]:
        if not self.control:
            return []
        # No output, command, path or PID enters the audit/channel.
        with self.control.sessions.lock:
            return [dict(id=s.identifier, completed=s.done.is_set(), reason=s.reason,
                         security_stop=self.stop_results.get(s.identifier, 'not_requested'),
                         output_withheld=not s.done.is_set() and self.policy['enabled'] and self.policy['response_guard'])
                    for s in self.control.sessions.sessions.values()]

    def _report(self) -> None:
        if self.channel:
            self.channel.report(dict(state='fault' if self.fault else 'ready', revision=revision(self.policy),
                                     lease_generation=self.lease.get('generation', -1),
                                     mode=self.mode, sessions=self._sessions(), counters=dict(self.counters),
                                     events=list(self.events), lost_events=self.lost_events))

    def tick(self) -> None:
        self.refresh()
        terminate = self.fault and self.protection_seen or (self.policy['enabled'] and self.policy['host_authorization'] and
                                   self.policy['session_expiry'] == 'terminate' and
                                   (not self.confirmed or self.lease.get('deadline', 0) <= time.monotonic()))
        if self.control and self.mode == 'host_control' and terminate:
            with self.control.sessions.lock:
                sessions = list(self.control.sessions.sessions.values())
            for session in sessions:
                if not session.done.is_set() and self.stop_results.get(session.identifier) != 'confirmed':
                    try:
                        with session.io_lock:
                            session.reason = 'security_authorization_lost'
                            session.sandbox.stop()
                        self.stop_results[session.identifier] = 'confirmed'
                        self.record('session_stopped')
                    except Exception:
                        self.fail()
                        if self.stop_results.get(session.identifier) != 'unconfirmed':
                            self.stop_results[session.identifier] = 'unconfirmed'
                            self.record('session_stop_failed', 'high')
        try:
            self._report()
        except Exception:
            self.fail()

    def start(self) -> None:
        def loop():
            while not self.stop_event.wait(.1):
                try:
                    self.tick()
                except Exception:
                    self.fail()
        self.thread = threading.Thread(target=loop, name='security-enforcement', daemon=True)
        self.thread.start()

    def close(self) -> None:
        self.stop_event.set()
        if self.thread:
            self.thread.join(timeout=2)
        if self.channel:
            try:
                self.channel.report(dict(state='closed', events=list(self.events), lost_events=self.lost_events))
            finally:
                self.channel.close()


def from_environment() -> SecurityRuntime:
    from mcp_local.security_policy import STATE_DIR
    return SecurityRuntime(STATE_DIR, os.environ.pop(ENV_NAME, None))
