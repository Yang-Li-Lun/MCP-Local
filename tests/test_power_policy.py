"""電源控制隔離測試：所有電源操作皆使用記憶體替身。"""
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
import uuid
from mcp_local.power_policy import PowerSchemeManager, PowerError
from legacy_power_fixture import LegacySchemeFixture, SLEEP, DISPLAY, EPP, MAXIMUM, BOOST
from mcp_local.power_restore_guard import serve


class FakePower:
    def __init__(self):
        self.original = str(uuid.uuid4())
        self.active = self.original
        self.schemes = {self.original}
        self.writes = []
        self.fail = None
        self.qos_values = []
        self.requests = 0
        self.releases = 0
        self.user_modes = {'AC': str(uuid.uuid4()), 'DC': str(uuid.uuid4())}
        self.initial_user_modes = self.user_modes.copy()
        self.user_supported = True
        self.user_fail = None
        self.user_writes = []

    def supports_user_power_mode(self):
        return self.user_supported

    def get_user_power_mode(self, supply):
        if self.user_fail == 'read_' + supply:
            raise PowerError('POWER_USER_MODE_READ_FAILED')
        return self.user_modes[supply]

    def set_user_power_mode(self, supply, value):
        if self.user_fail == 'set_' + supply:
            raise PowerError('POWER_USER_MODE_APPLY_FAILED')
        self.user_writes.append((supply, value))
        self.user_modes[supply] = value

    def get_active(self):
        return self.active

    def copy_scheme(self, source):
        value = str(uuid.uuid4())
        self.schemes.add(value)
        return value

    def set_active(self, scheme):
        if self.fail == 'activate':
            self.fail = None
            raise PowerError('POWER_SCHEME_APPLY_FAILED')
        if scheme not in self.schemes:
            raise PowerError('POWER_SCHEME_APPLY_FAILED')
        self.active = scheme

    def delete_scheme(self, scheme):
        if self.fail == 'delete':
            raise PowerError('POWER_SCHEME_DELETE_FAILED')
        self.schemes.discard(scheme)

    def current(self):
        return 123

    def process_identity(self, handle):
        return 321

    def read(self, scheme, group, setting, supply):
        if self.fail == 'capability' and (group, setting) == BOOST:
            raise PowerError('POWER_CAPABILITY_UNAVAILABLE')
        return 60 if supply == 'DC' else 0

    def write(self, scheme, group, setting, supply, value):
        if self.fail == 'write':
            raise PowerError('POWER_SCHEME_APPLY_FAILED')
        self.writes.append((group, setting, supply, value))

    def qos(self, enabled):
        if self.fail == 'qos':
            raise PowerError('POWER_QOS_APPLY_FAILED')
        self.qos_values.append(enabled)

    def request(self):
        if self.fail == 'request':
            raise PowerError('POWER_REQUEST_CREATE_FAILED')
        self.requests += 1
        return 111

    def release_request(self, handle):
        self.releases += 1

    def open_process(self, *args):
        return None


class PowerPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'power-runtime.json'
        self.api = FakePower()
        self.schemes = LegacySchemeFixture(self.api, self.path)

    def test_scheme_modes_restore_original_and_display_limits(self):
        self.schemes.apply('extreme')
        self.assertNotEqual(self.api.active, self.api.original)
        self.assertTrue(any(row[:2] == DISPLAY for row in self.api.writes))
        self.assertIn((*SLEEP, 'AC', 0), self.api.writes)
        self.assertIn((*EPP, 'DC', 100), self.api.writes)
        self.schemes.apply('extreme')
        self.assertIn((*DISPLAY, 'AC', 300), self.api.writes)
        self.assertIn((*DISPLAY, 'DC', 60), self.api.writes)
        self.assertIn((*MAXIMUM, 'AC', 80), self.api.writes)
        self.assertIn((*MAXIMUM, 'DC', 60), self.api.writes)
        self.assertIn((*BOOST, 'DC', 0), self.api.writes)
        self.schemes.apply('extreme')
        self.assertEqual(len(self.api.schemes), 2)
        self.schemes.restore()
        self.schemes.restore()
        self.assertEqual(self.api.schemes, {self.api.original})
        self.assertEqual(self.api.active, self.api.original)
        self.assertFalse(self.path.exists())

    def test_apply_failure_rolls_back_previous_mode(self):
        for failure in ('write', 'activate'):
            with self.subTest(failure=failure):
                self.schemes.apply('extreme')
                self.schemes.restore()
                before = self.api.active
                self.api.fail = failure
                with self.assertRaises(PowerError):
                    self.schemes.apply('extreme')
                self.assertEqual(self.api.active, before)
                self.assertIsNone(self.schemes.state)
                self.assertEqual(len(self.api.schemes), 1)
                self.api.fail = None
                self.schemes.restore()

    def test_journal_failure_does_not_activate_candidate(self):
        with patch.object(self.schemes, 'journal', side_effect=OSError('disk')):
            with self.assertRaises(OSError):
                self.schemes.apply('extreme')
        self.assertEqual(self.api.active, self.api.original)
        self.assertEqual(self.api.schemes, {self.api.original})

    def test_optional_capability_and_cleanup_failure_are_visible(self):
        self.api.fail = 'capability'
        self.assertTrue(self.schemes.apply('extreme'))
        self.api.fail = 'delete'
        with self.assertRaisesRegex(PowerError, 'DELETE_FAILED'):
            self.schemes.apply('extreme')
        with self.assertRaises(PowerError):
            self.schemes.restore()
        self.assertTrue(self.path.exists())
        self.api.fail = None
        self.schemes.restore()
        self.assertEqual(self.api.schemes, {self.api.original})

    def test_external_scheme_selection_is_respected(self):
        self.schemes.apply('extreme')
        external = str(uuid.uuid4())
        self.api.schemes.add(external)
        self.api.active = external
        self.schemes.restore()
        self.assertEqual(self.api.active, external)
        self.assertEqual(self.api.schemes, {external, self.api.original})

    def test_runtime_recovery_and_corruption(self):
        self.schemes.apply('extreme')
        PowerSchemeManager(self.api, self.path).recover()
        self.assertEqual(self.api.active, self.api.original)
        self.path.write_text('{bad', encoding='utf-8')
        with self.assertRaisesRegex(PowerError, 'CORRUPT'):
            PowerSchemeManager(self.api, self.path).recover()
        self.assertTrue(self.path.exists())

    def test_missing_original_retains_recovery_state(self):
        self.schemes.apply('extreme')
        self.api.schemes.remove(self.api.original)
        with self.assertRaisesRegex(PowerError, 'RESTORE_FAILED'):
            self.schemes.restore()
        self.assertTrue(self.path.exists())

    def test_live_owner_is_not_recovered(self):
        self.schemes.apply('extreme')
        with patch.object(self.api, 'open_process', return_value=123), \
             patch.object(self.api, 'wait_one', create=True, return_value=258), \
             patch.object(self.api, 'close', create=True):
            with self.assertRaisesRegex(PowerError, 'ALREADY_RUNNING'):
                PowerSchemeManager(self.api, self.path).recover()
        self.assertNotEqual(self.api.active, self.api.original)

    def test_guard_recovers_journal_and_rejects_new_mode(self):
        self.schemes.apply('extreme')
        recovered = PowerSchemeManager(self.api, self.path)
        output = io.StringIO()
        with patch('ctypes.get_last_error', return_value=0):
            serve(recovered, io.StringIO('{"mode":"extreme"}\n{"mode":"off"}\n'), output)
        responses = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertTrue(responses[0]['ok'])
        self.assertFalse(responses[1]['ok'])
        self.assertTrue(responses[2]['ok'])
        self.assertEqual(self.api.active, self.api.original)
        self.assertFalse(self.path.exists())

    def test_guard_broken_parent_output_still_restores(self):
        self.schemes.apply('extreme')
        recovered = PowerSchemeManager(self.api, self.path)
        output = Mock()
        output.write.side_effect = [None, BrokenPipeError()]
        with patch('ctypes.get_last_error', return_value=0), self.assertRaises(BrokenPipeError):
            serve(recovered, io.StringIO('{"mode":"off"}\n'), output)
        self.assertEqual(self.api.active, self.api.original)

class PowerApiBoundaryTests(unittest.TestCase):
    def test_guard_environment_excludes_credentials(self):
        import os
        from mcp_local.power_policy import PowerRestoreGuardClient
        process = Mock(stdin=io.StringIO(), stdout=io.StringIO('{"ok":true}\n'))
        with patch.dict(os.environ, {'CONTROL_PLANE_API_KEY': 'FAKE_DO_NOT_INHERIT', 'OPENAI_API_KEY': 'FAKE'}), \
             patch('mcp_local.power_policy.subprocess.Popen', return_value=process) as spawn:
            guard = PowerRestoreGuardClient(Path('unused-power-test.json'))
            guard.start()
        environment = spawn.call_args.kwargs['env']
        self.assertNotIn('CONTROL_PLANE_API_KEY', environment)
        self.assertNotIn('OPENAI_API_KEY', environment)
        self.assertTrue(spawn.call_args.kwargs['close_fds'])
        guard.abort()
