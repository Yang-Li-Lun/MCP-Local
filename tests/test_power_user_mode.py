"""Windows 使用者電源模式：記憶體替身，不改動系統。"""
import ctypes
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
import uuid
from test_power_policy import FakePower
from mcp_local.power_policy import PowerSchemeManager, PowerError
from legacy_power_fixture import LegacySchemeFixture
from mcp_local.power_windows import WindowsPower, Guid, GUID_POWER_MODE_BEST_EFFICIENCY as BEST
from mcp_local.connection_settings import normalize_connection, load_settings
from mcp_local.power_restore_guard import serve


class UserModeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'runtime.json'
        self.api = FakePower()
        self.manager = LegacySchemeFixture(self.api, self.path)

    def test_intent_is_durable_before_either_write(self):
        setter = self.api.set_user_power_mode
        def check(supply, value):
            journal = json.loads(self.path.read_text())
            self.assertEqual(journal['journal_version'], 2)
            self.assertEqual(journal['original_ac_power_mode'], self.api.initial_user_modes['AC'])
            setter(supply, value)
        self.api.set_user_power_mode = check
        self.assertEqual(self.manager.apply('extreme'), [])
        self.assertEqual(self.api.user_modes, {'AC': BEST, 'DC': BEST})
        self.manager.restore()
        self.assertEqual(self.api.user_modes, self.api.initial_user_modes)
        self.assertFalse(self.path.exists())

    def test_unavailable_and_read_failures_skip_all_user_writes(self):
        for failure in ('unavailable', 'read_AC', 'read_DC'):
            with self.subTest(failure=failure):
                self.api.user_supported = failure != 'unavailable'
                self.api.user_fail = failure
                warnings = self.manager.apply('extreme')
                self.assertIn('POWER_USER_MODE_UNAVAILABLE' if failure == 'unavailable'
                              else 'POWER_USER_MODE_READ_FAILED', warnings)
                self.assertNotEqual(self.api.active, self.api.original)
                self.assertEqual(self.api.user_writes, [])
                self.manager.restore()

    def test_partial_apply_failures_remain_recoverable(self):
        for supply in ('AC', 'DC'):
            with self.subTest(supply=supply):
                self.api.user_fail = 'set_' + supply
                self.assertIn('POWER_USER_MODE_APPLY_FAILED:' + supply,
                              self.manager.apply('extreme'))
                self.assertEqual(self.api.user_modes[supply], self.api.initial_user_modes[supply])
                self.api.user_fail = None
                PowerSchemeManager(self.api, self.path).recover()
                self.assertEqual(self.api.user_modes, self.api.initial_user_modes)
                self.manager.state = None

    def test_external_user_selection_is_preserved_independently(self):
        self.manager.apply('extreme')
        external = str(uuid.uuid4())
        self.api.user_modes['AC'] = external
        self.manager.restore()
        self.assertEqual(self.api.user_modes['AC'], external)
        self.assertEqual(self.api.user_modes['DC'], self.api.initial_user_modes['DC'])

    def test_restore_failure_retains_journal_and_retries(self):
        self.manager.apply('extreme')
        self.api.user_fail = 'set_AC'
        with self.assertRaisesRegex(PowerError, 'USER_MODE_RESTORE_FAILED'):
            self.manager.restore()
        self.assertTrue(self.path.exists())
        self.assertEqual(self.api.user_modes['DC'], self.api.initial_user_modes['DC'])
        self.api.user_fail = None
        PowerSchemeManager(self.api, self.path).recover()
        self.assertEqual(self.api.user_modes, self.api.initial_user_modes)
        self.assertEqual(self.api.schemes, {self.api.original})

    def test_options_independently_disable_controls(self):
        self.manager.apply('extreme', manage_power_scheme=False)
        self.assertEqual(self.api.schemes, {self.api.original})
        self.assertEqual(self.api.user_modes, {'AC': BEST, 'DC': BEST})
        PowerSchemeManager(self.api, self.path).recover()
        self.manager.state = None
        self.manager.apply('extreme', manage_windows_power_mode=False)
        self.assertEqual(self.api.user_modes, self.api.initial_user_modes)
        self.manager.restore()

    def test_legacy_low_journal_recovers_without_runtime_low(self):
        self.manager.apply('extreme', manage_windows_power_mode=False)
        state = {k: self.manager.state[k] for k in
                 ('owner_pid', 'owner_created', 'original', 'owned', 'mode')}
        state['mode'] = 'low'
        self.path.write_text(json.dumps(state))
        PowerSchemeManager(self.api, self.path).recover()
        self.assertEqual(self.api.active, self.api.original)
        with self.assertRaises(ValueError):
            self.manager.apply('low')

    def test_corrupt_v2_never_mutates_system(self):
        self.manager.apply('extreme')
        saved = json.loads(self.path.read_text())
        for field, bad in (('journal_version', True), ('original_ac_power_mode', 'bad'),
                           ('user_power_mode_supported', 1), ('extra', 1),
                           ('applied_power_mode', str(uuid.uuid4()))):
            self.path.write_text(json.dumps({**saved, field: bad}))
            before = self.api.user_modes.copy()
            with self.assertRaisesRegex(PowerError, 'CORRUPT'):
                PowerSchemeManager(self.api, self.path).recover()
            self.assertEqual(self.api.user_modes, before)
        self.path.write_text(json.dumps(saved))
        self.manager.restore()

    def test_corrupt_encoding_size_duplicate_keys_and_guid_alias(self):
        self.manager.apply('extreme')
        saved = json.loads(self.path.read_text())
        alias = {**saved, 'owned': [saved['original'].upper()]}
        for data in (b'\xff', b' ' * 4097, b'{"mode":"off","mode":"extreme"}',
                     json.dumps(alias).encode()):
            self.path.write_bytes(data)
            before = self.api.user_modes.copy()
            with self.assertRaisesRegex(PowerError, 'CORRUPT'):
                PowerSchemeManager(self.api, self.path).recover()
            self.assertEqual(self.api.user_modes, before)
        self.path.write_text(json.dumps(saved))
        self.manager.restore()

    def test_journal_link_is_rejected_before_reads_or_mutations(self):
        self.manager.apply('extreme')
        before = self.api.user_modes.copy()
        with patch.object(Path, 'is_symlink', return_value=True):
            with self.assertRaisesRegex(PowerError, 'CORRUPT'):
                PowerSchemeManager(self.api, self.path).recover()
        self.assertEqual(self.api.user_modes, before)
        self.manager.restore()

    def test_display_zero_caps_and_shorter_values(self):
        from legacy_power_fixture import DISPLAY, EPP, SLEEP
        for value, expected in ((0, (300, 120)), (30, (30, 30)), (900, (300, 120))):
            with patch.object(self.api, 'read', return_value=value):
                self.manager.apply('extreme')
            for supply, cap in zip(('AC', 'DC'), expected):
                self.assertIn((*DISPLAY, supply, cap), self.api.writes)
                self.assertIn((*SLEEP, supply, 0), self.api.writes)
            self.assertIn((*EPP, 'AC', 90), self.api.writes)
            self.manager.restore()
            self.api.writes.clear()

    def test_guard_rejects_new_power_mode(self):
        output = io.StringIO()
        serve(self.manager, io.StringIO(json.dumps({'mode': 'extreme'}) + '\n'), output)
        responses = [json.loads(row) for row in output.getvalue().splitlines()]
        self.assertTrue(responses[0]['ok'])
        self.assertFalse(responses[1]['ok'])
        self.assertEqual(self.api.schemes, {self.api.original})
        self.assertEqual(self.api.user_modes, self.api.initial_user_modes)

    def test_legacy_modes_are_retired_preserving_roots(self):
        root = Path(self.temp.name).resolve()
        base = {'roots': [{'id': 'main', 'name': 'main', 'path': str(root),
                          'excluded_names': ['private']}],
                'default_root': 'main', 'tunnel': 'tunnel_fixture'}
        target = root / 'settings.json'
        for old in ('off', 'low', 'extreme'):
            target.write_text(json.dumps({**base, 'settings_version': 4, 'power': {'mode': old}}))
            result = load_settings(target)
            self.assertNotIn('power', result)
            self.assertEqual(result['roots'], base['roots'])
            self.assertNotIn('power', json.loads(target.read_text(encoding='utf-8')))
        data = json.dumps({**base, 'settings_version': 99})
        target.write_text(data)
        with self.assertRaises(ValueError):
            load_settings(target)
        self.assertEqual(target.read_text(), data)

class BindingTests(unittest.TestCase):
    def test_optional_bindings_missing(self):
        kernel = Mock()
        powr = Mock()
        for supply in ('AC', 'DC'):
            setattr(powr, 'PowerGetUserConfigured' + supply + 'PowerMode', None)
            setattr(powr, 'PowerSetUserConfigured' + supply + 'PowerMode', None)
        with patch('mcp_local.power_windows.c.WinDLL', side_effect=[kernel, powr]):
            api = WindowsPower()
        self.assertFalse(api.supports_user_power_mode())
        with self.assertRaisesRegex(PowerError, 'UNAVAILABLE'):
            api.get_user_power_mode('AC')

    def test_bindings_and_mutation_audit(self):
        kernel, powr = Mock(), Mock()
        for supply in ('AC', 'DC'):
            def getter(pointer):
                ctypes.memmove(pointer, ctypes.byref(Guid.parse(BEST)), ctypes.sizeof(Guid))
                return 0
            getattr(powr, 'PowerGetUserConfigured' + supply + 'PowerMode').side_effect = getter
            getattr(powr, 'PowerSetUserConfigured' + supply + 'PowerMode').return_value = 0
        with patch('mcp_local.power_windows.c.WinDLL', side_effect=[kernel, powr]):
            api = WindowsPower()
        self.assertTrue(api.supports_user_power_mode())
        with patch('sys.audit') as audit:
            for supply in ('AC', 'DC'):
                self.assertEqual(api.get_user_power_mode(supply), BEST)
                api.set_user_power_mode(supply, BEST)
                audit.assert_called_with('mcp.power.mutate', 'PowerSetUserConfigured' + supply + 'PowerMode')

