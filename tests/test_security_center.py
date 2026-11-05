"""安全中心：合成政策、簽章通訊、真正 STDIO 送出及受控主機 Job。"""
from __future__ import annotations

import asyncio
import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch

from mcp_local.policy import PolicyStore, defaults, normalize, revision
from mcp_local.host_authorization import HostAuthorization, require_authorized
from mcp_local.local_channel import LocalChannel, ENV_NAME
from mcp_local.response_guard import ResponseGuard, GuardedOutput
from mcp_local.security_center import SecurityCenter, SecurityRuntime
from mcp_local.security_audit import SecurityAudit, event
from mcp_local.sensitive_detector import SYNTHETIC, SensitiveDetector

ROOT = Path(__file__).resolve().parents[1]
BOOTSTRAP = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from mcp_local import security_policy
security_policy.STATE_DIR = Path(sys.argv[2])
from mcp_local import developer_control
developer_control.backend_status = lambda toolchains=None: {'ready': False, 'reason': 'SYNTHETIC', 'host_fallback': False}
from mcp_local import developer_toolchains
developer_toolchains.discover_toolchains = lambda roots=None: []
from mcp_local.local_files_mcp import main
sys.argv = ['local_files_mcp.py', '--root', sys.argv[3], '--access-mode', sys.argv[4]]
main()
'''


class PolicyAndGuardTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='mcp-security-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.policy = {**defaults(), 'enabled': True}

    def test_legacy_defaults_no_write_atomic_save_conflict_and_unknown_version(self):
        store = PolicyStore(self.root)
        value, stamp = store.read()
        self.assertFalse(value['enabled'])
        self.assertEqual(stamp, '')
        self.assertFalse(store.path.exists())
        _, saved = store.save(self.policy, '')
        with self.assertRaisesRegex(ValueError, 'CONFLICT'):
            store.save(defaults(), '')
        self.assertEqual(store.read()[1], saved)
        store.path.write_text('{"version":999}', encoding='utf-8')
        before = store.path.read_bytes()
        with self.assertRaisesRegex(ValueError, 'INVALID'):
            store.read()
        with self.assertRaisesRegex(ValueError, 'INVALID'):
            store.save(defaults(), saved)
        self.assertEqual(store.path.read_bytes(), before)
        self.assertFalse(list(self.root.glob('*.tmp')))
        self.assertFalse(list(self.root.glob('*.lock')))

    def test_policy_rejects_dependencies_types_unknown_rules_and_limits(self):
        for changes in ({'detector_enabled': False}, {'max_lease_minutes': True}, {'strategy': 'allow'},
                        {'enabled': 'yes'}, {'rules': []}, {'version': 2}):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                normalize({**self.policy, **changes})
        bad = {**self.policy, 'rules': [dict(id='custom', kind='regex', pattern='(a+)+', enabled=True, risk='high')]}
        with self.assertRaises(ValueError):
            normalize(bad)

    def test_monotonic_lease_expiry_revoke_and_no_implicit_renewal(self):
        clock = Mock(return_value=10)
        auth = HostAuthorization(clock)
        auth.grant(1, 2)
        self.assertEqual(auth.snapshot()['remaining_seconds'], 60)
        require_authorized(self.policy, auth.snapshot(), True, now=69)
        for now, confirmed in ((70, True), (20, False)):
            with self.assertRaisesRegex(ValueError, 'HOST_AUTH_REQUIRED'):
                require_authorized(self.policy, auth.snapshot(), confirmed, now)
        auth.revoke()
        self.assertFalse(auth.snapshot()['authorized'])

    def test_detector_rule_management_and_risk_without_original_findings(self):
        masked, findings = SensitiveDetector(self.policy).inspect(SYNTHETIC)
        self.assertNotIn('synthetic_password', masked)
        self.assertTrue(any(row['risk'] == 'high' for row in findings))
        self.assertNotIn('synthetic', json.dumps(findings))
        policy = {**self.policy, 'rules': [dict(id='custom', kind='literal', pattern='test-marker', enabled=True, risk='low')]}
        self.assertEqual(SensitiveDetector(policy).inspect('test-marker')[1][0]['risk'], 'low')
        policy['rules'][0]['enabled'] = False
        self.assertEqual(SensitiveDetector(policy).inspect('test-marker')[0], 'test-marker')

    def test_guard_nested_text_json_keys_context_and_block(self):
        payload = dict(content=[dict(type='text', text=json.dumps({'password': 'synthetic-value', 'nested': ['sk-synthetic_1234567890']}))],
                       structuredContent={'secret': 'another-synthetic', 'Bearer synthetic_token_12345': 'ordinary'})
        result = ResponseGuard(self.policy).inspect(payload)
        self.assertEqual(result['action'], 'masked')
        rendered = json.dumps(result['payload'])
        self.assertNotIn('synthetic-value', rendered)
        self.assertNotIn('another-synthetic', rendered)
        self.assertNotIn('synthetic_token', rendered)
        self.assertEqual(payload['structuredContent']['secret'], 'another-synthetic')
        self.assertEqual(ResponseGuard({**self.policy, 'strategy': 'block'}).inspect(payload)['action'], 'blocked')

    def test_guard_rejects_binary_unknown_deep_oversize_without_safe_claim(self):
        deep = {}
        for _ in range(40):
            deep = {'x': deep}
        for payload in ({'type': 'image', 'data': 'base64'}, {'encoding': 'base64', 'data': 'c2VjcmV0'},
                        {'resource': {'blob': 'xx'}}, {'x': object()}, {'x': 'a' * (4 * 1024 * 1024 + 1)}, deep):
            with self.subTest(kind=type(payload)):
                result = ResponseGuard(self.policy).inspect(payload)
                self.assertEqual(result['action'], 'blocked')
                self.assertFalse(result['checked'])

    def test_long_and_incomplete_private_key_and_quoted_password_are_fully_masked(self):
        detector = SensitiveDetector(self.policy)
        for text in ('-----BEGIN PRIVATE KEY-----' + 'x' * 70000,
                     'Bearer ' + 'x' * 5000, 'sk-' + 'x' * 2000,
                     'password="synthetic phrase with spaces"'):
            masked, found = detector.inspect(text)
            self.assertTrue(found)
            self.assertNotIn('x', masked)
            self.assertNotIn('spaces', masked)

    def test_disabled_preserves_exact_response_and_normal_developer_text(self):
        payload = {'text': SYNTHETIC}
        self.assertIs(ResponseGuard(defaults()).inspect(payload)['payload'], payload)
        normal = {'text': 'Get-ChildItem; python -V; git status; dotnet build'}
        self.assertEqual(ResponseGuard(self.policy).inspect(normal)['payload'], normal)

    def test_guard_actual_transport_errors_notifications_and_metadata(self):
        from mcp import types
        from mcp.shared.message import SessionMessage
        class Sink:
            def __init__(self): self.messages = []
            async def send(self, value): self.messages.append(value)
        runtime = SecurityRuntime(self.root)
        runtime.policy = self.policy
        runtime.refresh = lambda: None
        sink = Sink()
        stream = GuardedOutput(sink, runtime)
        for root in (types.JSONRPCError(jsonrpc='2.0', id=3, error=types.ErrorData(code=-1, message=SYNTHETIC, data={'password': 'synthetic-value'})),
                     types.JSONRPCNotification(jsonrpc='2.0', method='notifications/message', params={'text': SYNTHETIC})):
            asyncio.run(stream.send(SessionMessage(types.JSONRPCMessage(root))))
        self.assertEqual(len(sink.messages), 2)
        self.assertNotIn('synthetic_password', str(sink.messages))
        self.assertEqual(sink.messages[0].message.root.error.code, -1)

    def test_audit_filter_export_tamper_and_controlled_clear(self):
        audit = SecurityAudit(self.root)
        audit.append(event('response_blocked', risk='high'))
        audit.append(event('synthetic_test'))
        self.assertEqual(audit.query(risk='high')['total'], 1)
        snapshot = audit.export()
        self.assertFalse(snapshot['contains_sensitive_originals'])
        with self.assertRaisesRegex(ValueError, 'CONFIRM'):
            audit.clear(snapshot['revision'], 'yes')
        with self.assertRaisesRegex(ValueError, 'CONFLICT'):
            audit.clear('0' * 64, '清除安全事件')
        audit.clear(snapshot['revision'], '清除安全事件')
        self.assertEqual(audit.query()['rows'][0]['event']['kind'], 'audit_cleared')
        audit.path.write_text(audit.path.read_text().replace('audit_cleared', 'synthetic_test'))
        with self.assertRaisesRegex(ValueError, 'INTEGRITY'):
            audit.query()

    def test_deleted_enabled_policy_stays_fail_closed(self):
        store = PolicyStore(self.root)
        store.save(self.policy, '')
        runtime = SecurityRuntime(self.root)
        store.path.unlink()
        runtime.refresh()
        self.assertTrue(runtime.fault)
        with self.assertRaises(ValueError):
            runtime.require_host()
        self.assertTrue(runtime.protect({'text': 'ordinary'})['isError'])


@unittest.skipUnless(os.name == 'nt', 'Windows local mapping')
class ChannelTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='mcp-security-channel-')
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.center = SecurityCenter(self.root)
        self.addCleanup(self.center.close)
        self.assertIsNone(self.center.error)

    def client(self):
        value = SecurityRuntime(self.root, self.center.environment()[ENV_NAME])
        self.addCleanup(value.close)
        value.mode = 'host_control'
        value.tick()
        self.center.tick()
        return value

    def test_public_client_cannot_sign_grant_and_tamper_latches_until_recovery(self):
        runtime = self.client()
        self.assertIsNone(runtime.channel.private)
        with self.assertRaisesRegex(ValueError, 'LOCAL_ONLY'):
            runtime.channel.publish(defaults(), '', {}, healthy=True)
        self.center.policy, self.center.saved_revision = self.center.store.save({**defaults(), 'enabled': True}, '')
        self.center._publish()
        runtime.tick(); self.center.tick()
        self.center.grant(1)
        runtime.require_host()
        with self.center.channel.locked():
            self.center.channel.memory[10] ^= 1
        runtime.refresh()
        self.assertTrue(runtime.fault)
        self.center._publish()
        runtime.refresh()
        self.assertTrue(runtime.fault)
        self.center.recover()
        runtime.refresh()
        self.assertFalse(runtime.fault)
        with self.assertRaisesRegex(ValueError, 'HOST_AUTH_REQUIRED'):
            runtime.require_host()

    def test_stale_signed_frame_replay_rejected_and_does_not_extend_lease(self):
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        runtime.refresh()
        self.center.stop_event.set(); self.center.thread.join(2)
        value = runtime.channel.read_control()
        with self.center.channel.locked():
            self.center.channel._write(0, {**value, 'time': time.monotonic() - 5}, True)
        runtime.refresh()
        self.assertTrue(runtime.fault)
        with self.assertRaises(ValueError):
            runtime.require_host()

    def test_disabled_policy_does_not_change_tools_on_channel_fault(self):
        runtime = self.client()
        runtime.channel.close()
        runtime.refresh()
        self.assertTrue(runtime.fault)
        runtime.require_host()
        payload = {'text': SYNTHETIC}
        self.assertEqual(runtime.protect(payload), payload)
        # Close is already performed by this test; cleanup only owns the thread.
        runtime.channel = None

    def test_save_revoke_ack_events_and_recover_does_not_grant(self):
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        runtime.tick(); self.center.tick()
        self.center.grant(1)
        runtime.require_host()
        runtime.protect({'text': SYNTHETIC})
        self.center.tick()
        self.assertGreater(self.center.audit.query()['total'], 0)
        self.assertNotIn('synthetic_password', self.center.audit.path.read_text())
        self.center.revoke()
        with self.assertRaises(ValueError): runtime.require_host()
        self.center.recover(); runtime.tick(); self.center.tick()
        self.assertFalse(self.center.authorization.snapshot()['authorized'])
        self.assertEqual(self.center.snapshot()['effective'], 'confirmed')

    def test_unconfirmed_client_cannot_grant_or_claim_effective(self):
        with self.assertRaises(ValueError): self.center.grant(1)
        self.assertEqual(self.center.snapshot()['effective'], 'no_client')

    def test_recovery_cannot_accept_external_policy_downgrade(self):
        self.center.save({**defaults(), 'enabled': True}, '')
        self.center.store.save(defaults(), self.center.saved_revision)
        self.center.tick()
        self.assertIsNotNone(self.center.error)
        with self.assertRaisesRegex(ValueError, 'POLICY_CHANGED'):
            self.center.recover()
        self.assertTrue(self.center.policy['enabled'])
        _, external_revision = self.center.store.read()
        self.center.store.save(self.center.policy, external_revision)
        self.center.recover()
        self.assertIsNone(self.center.error)

    def test_session_partial_output_and_page_boundary_never_leak(self):
        from mcp_local.control_sessions import Session
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        runtime.refresh()
        session = Session(Mock(pid=123), output_filter=runtime.session_output)
        session.append('Bearer synthetic')
        self.assertEqual(session.view(length=8)['output'], '')
        session.append('_token_123456789')
        session.done.set()
        self.assertNotIn('Bearer', session.view(length=8)['output'])
        self.assertNotIn('synthetic', session.view(offset=4)['output'])
        session.dropped = 1
        self.assertIn('SECURITY_RESPONSE_BLOCKED', session.view()['output'])

    def test_expiry_terminate_and_finish_policy_with_mock_sessions(self):
        from mcp_local.control_sessions import Session
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        runtime.tick(); self.center.tick()
        process = Mock(pid=123)
        session = Session(process)
        runtime.control = Mock(sessions=Mock(lock=threading.RLock(), sessions={'one': session}))
        runtime.tick()
        process.stop.assert_called_once()
        self.assertEqual(session.reason, 'security_authorization_lost')
        self.center.save({**self.center.policy, 'session_expiry': 'finish'}, self.center.saved_revision)
        process.stop.reset_mock()
        runtime.tick()
        process.stop.assert_not_called()
        with self.assertRaises(ValueError): runtime.require_host()

    def test_stop_failure_is_latched_and_reported_until_local_recovery(self):
        from mcp_local.control_sessions import Session
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        runtime.tick(); self.center.tick()
        process = Mock(pid=123, stop=Mock(side_effect=ValueError('synthetic stop failure')))
        session = Session(process)
        runtime.control = Mock(sessions=Mock(lock=threading.RLock(), sessions={'one': session}))
        runtime.tick()
        self.assertTrue(runtime.fault)
        runtime.refresh()
        self.assertTrue(runtime.fault)
        self.assertEqual(runtime._sessions()[0]['security_stop'], 'unconfirmed')
        self.assertFalse(runtime._sessions()[0]['completed'])

    def test_pending_audit_overflow_is_explicit_and_requires_local_recovery(self):
        runtime = self.client()
        for _ in range(128): runtime.record('synthetic_test')
        with self.assertRaisesRegex(ValueError, 'BACKPRESSURE'):
            runtime.record('synthetic_test')
        self.assertTrue(runtime.fault)
        runtime.tick(); self.center.tick()
        self.assertIsNotNone(self.center.error)
        self.assertEqual(self.center.audit.query(kind='events_lost')['total'], 1)
        self.center.recover(); runtime.refresh()
        self.assertFalse(runtime.fault)

    def test_monitor_keeps_request_evidence_and_observes_guarded_response(self):
        from mcp import types
        from mcp.shared.message import SessionMessage
        from mcp_local.request_monitor import Monitor, MemoryChannel, Observer, ObservedOutput
        monitor = Monitor()
        monitor.begin_connection()
        self.addCleanup(monitor.close)
        channel = MemoryChannel(monitor.channel.token)
        self.addCleanup(channel.close)
        observer = Observer(channel)
        runtime = self.client()
        self.center.save({**defaults(), 'enabled': True}, '')
        request = SessionMessage(types.JSONRPCMessage(types.JSONRPCRequest(jsonrpc='2.0', id=1, method='tools/call', params={'name': 'start_process', 'arguments': {'command': SYNTHETIC}})))
        observer.receive(request)
        class Sink:
            async def send(self, value): pass
        stream = GuardedOutput(ObservedOutput(Sink(), observer), runtime)
        response = SessionMessage(types.JSONRPCMessage(types.JSONRPCResponse(jsonrpc='2.0', id=1, result={'content': [{'type': 'text', 'text': SYNTHETIC}]})))
        asyncio.run(stream.send(response))
        for _ in range(4): monitor.poll()
        row = list(monitor.records.values())[0]
        self.assertIn('synthetic_password', str(row['request']))
        self.assertNotIn('synthetic_password', str(row['response']))


@unittest.skipUnless(os.name == 'nt', 'Windows native host/STDIO')
class SecurityStdioTests(unittest.TestCase):
    def test_three_contracts_and_real_mask_block_session_revoke_recovery(self):
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        from mcp_local.tool_contract import verify_contract
        with tempfile.TemporaryDirectory(prefix='mcp-security-stdio-') as temporary:
            base = Path(temporary)
            state, root = base / 'state', base / 'root'
            root.mkdir()
            (root / 'sensitive.txt').write_text(SYNTHETIC, encoding='utf-8')
            (root / 'normal.txt').write_text('synthetic ordinary text', encoding='utf-8')
            center = SecurityCenter(state)
            try:
                center.save({**defaults(), 'enabled': True}, '')
                async def verify(mode):
                    parameters = StdioServerParameters(command=sys.executable,
                        args=['-I', '-B', '-c', BOOTSTRAP, str(ROOT), str(state), str(root), mode], env=center.environment())
                    async with stdio_client(parameters) as (read, write):
                        async with ClientSession(read, write) as client:
                            await client.initialize()
                            listed = await client.list_tools()
                            self.assertEqual(len(listed.tools), {'read_only': 22, 'developer_control': 34, 'host_control': 36}[mode])
                            verify_contract(listed.tools, mode)
                            result = await client.call_tool('read_file', {'path': 'sensitive.txt'})
                            self.assertFalse(result.isError)
                            self.assertNotIn('synthetic_password', result.model_dump_json())
                            self.assertNotIn('synthetic_token', result.model_dump_json())
                            binary = await client.call_tool('read_binary', {'path': 'sensitive.txt'})
                            self.assertTrue(binary.isError)
                            if mode == 'host_control':
                                denied = await client.call_tool('start_process', {'command': "Write-Output 'ordinary'"})
                                self.assertTrue(denied.isError)
                                for _ in range(40):
                                    center.tick()
                                    if center.snapshot()['effective'] == 'confirmed': break
                                    await asyncio.sleep(.1)
                                center.grant(1)
                                positive = await client.call_tool('start_process', {'command': "Write-Output 'synthetic-powershell'; & '" + sys.executable + "' -B -c \"print('synthetic-python')\"; git --version", 'timeout_ms': 1000})
                                positive_data = positive.structuredContent or json.loads(positive.content[0].text)
                                for _ in range(30):
                                    if positive_data.get('completed'): break
                                    positive = await client.call_tool('read_process_output', {'session_id': positive_data['session_id'], 'wait_ms': 100})
                                    positive_data = positive.structuredContent or json.loads(positive.content[0].text)
                                self.assertIn('synthetic-powershell', positive_data['output'])
                                self.assertIn('synthetic-python', positive_data['output'])
                                self.assertIn('git version', positive_data['output'])
                                await client.call_tool('close_session', {'session_id': positive_data['session_id']})
                                command = "[Console]::WriteLine('Bearer synthetic_token_123456789'); Start-Sleep -Seconds 20"
                                started = await client.call_tool('start_process', {'command': command, 'timeout_ms': 100})
                                self.assertFalse(started.isError, started)
                                data = started.structuredContent or json.loads(started.content[0].text)
                                self.assertEqual(data['output'], '')
                                sid = data['session_id']
                                center.revoke()
                                interaction = await client.call_tool('interact_with_process', {'session_id': sid, 'input': 'x'})
                                self.assertTrue(interaction.isError)
                                for _ in range(60):
                                    ended = await client.call_tool('read_process_output', {'session_id': sid, 'wait_ms': 100})
                                    data = ended.structuredContent or json.loads(ended.content[0].text)
                                    if data.get('completed'): break
                                    await asyncio.sleep(.1)
                                self.assertTrue(data['completed'])
                                self.assertEqual(data['reason'], 'security_authorization_lost')
                                self.assertNotIn('synthetic_token', ended.model_dump_json())
                                closed = await client.call_tool('close_session', {'session_id': sid})
                                self.assertFalse(closed.isError)
                                center.grant(1)
                                # Synthetic short deadline exercises the native expiry path
                                # without changing the public 1-minute minimum.
                                center.authorization.deadline = time.monotonic() + .5
                                center._publish()
                                expires = await client.call_tool('start_process', {'command': 'Start-Sleep -Seconds 20', 'timeout_ms': 0})
                                expires_data = expires.structuredContent or json.loads(expires.content[0].text)
                                self.assertFalse(expires.isError)
                                for _ in range(50):
                                    result = await client.call_tool('read_process_output', {'session_id': expires_data['session_id'], 'wait_ms': 100})
                                    data = result.structuredContent or json.loads(result.content[0].text)
                                    if data.get('completed'): break
                                    await asyncio.sleep(.05)
                                self.assertTrue(data['completed'])
                                self.assertEqual(data['reason'], 'security_authorization_lost')
                                self.assertTrue((await client.call_tool('start_process', {'command': 'Write-Output denied'})).isError)
                                await client.call_tool('close_session', {'session_id': expires_data['session_id']})
                            center.save({**center.policy, 'strategy': 'block'}, center.saved_revision)
                            blocked_result = await client.call_tool('read_file', {'path': 'sensitive.txt'})
                            self.assertTrue(blocked_result.isError)
                            center.save({**center.policy, 'strategy': 'mask'}, center.saved_revision)
                    await asyncio.sleep(.1)
                    center.tick()
                for mode in ('read_only', 'developer_control', 'host_control'):
                    with self.subTest(mode=mode): asyncio.run(verify(mode))
            finally:
                center.close()


if __name__ == '__main__':
    unittest.main()
