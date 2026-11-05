from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import TestCase
from unittest.mock import patch, MagicMock
import json
from mcp_local.legacy_power_recovery import recover_legacy_power
from mcp_local.connection_settings import normalize_connection, save_settings, load_for_edit, retire_legacy_power_setting

class RetirementTests(TestCase):
    def test_absent_journal_never_starts_guard(self):
        with TemporaryDirectory() as name, patch('mcp_local.power_policy.PowerRestoreGuardClient') as manager:
            recover_legacy_power(Path(name))
            manager.assert_not_called()

    def test_recovery_runs_and_failure_stops(self):
        with TemporaryDirectory() as name:
            journal = Path(name) / 'power-runtime.json'
            journal.write_text('{}')
            with patch('mcp_local.power_policy.PowerRestoreGuardClient') as factory:
                manager = factory.return_value
                manager.close.side_effect = lambda: journal.unlink()
                recover_legacy_power(Path(name))
                manager.start.assert_called_once()
                manager.close.assert_called_once()
            journal.write_text('{}')
            with patch('mcp_local.power_policy.PowerRestoreGuardClient') as factory:
                manager = factory.return_value
                manager.start.side_effect = RuntimeError('recovery failed')
                with self.assertRaisesRegex(RuntimeError, 'recovery failed'):
                    recover_legacy_power(Path(name))
                manager.abort.assert_called_once()
                self.assertTrue(journal.exists())

    def test_journal_must_be_removed_to_proceed(self):
        with TemporaryDirectory() as name:
            journal = Path(name) / 'power-runtime.json'
            journal.write_text('{}')
            with patch('mcp_local.power_policy.PowerRestoreGuardClient'):
                with self.assertRaisesRegex(RuntimeError, 'POWER_SCHEME_RESTORE_FAILED'):
                    recover_legacy_power(Path(name))
            self.assertTrue(journal.exists())

    def test_legacy_settings_are_read_and_safe_save_drops_power(self):
        with TemporaryDirectory() as name:
            root = Path(name)
            old = {'settings_version': 8, 'roots': [{'id':'main','name':'main','path':name}],
                   'default_root':'main', 'tunnel':'tunnel_fixture', 'power':{'mode':'extreme'}}
            path = root / 'settings.json'; path.write_text(json.dumps(old))
            editable = load_for_edit(path)
            self.assertNotIn('power', editable.settings)
            self.assertTrue(retire_legacy_power_setting(path))
            self.assertFalse(retire_legacy_power_setting(path))
            saved = json.loads(path.read_text(encoding='utf-8'))
            self.assertNotIn('power', saved)
            self.assertEqual(saved['tunnel'], old['tunnel'])
            self.assertEqual(saved['roots'][0]['path'], str(root.resolve()))
            self.assertTrue(list(root.glob('settings.json.*.bak')))

    def test_setting_conflict_or_invalid_root_preserves_original(self):
        with TemporaryDirectory() as name:
            root = Path(name)
            original = {'settings_version': 8, 'roots': [{'id':'main','name':'main','path':str(root / 'missing')}],
                        'default_root':'main', 'tunnel':'tunnel_fixture', 'power':{'mode':'extreme'}}
            path = root / 'settings.json'; path.write_text(json.dumps(original))
            before = path.read_bytes()
            self.assertFalse(retire_legacy_power_setting(path))
            self.assertEqual(before, path.read_bytes())
            original['roots'][0]['path'] = name
            path.write_text(json.dumps(original))
            before = path.read_bytes()
            with patch('mcp_local.connection_settings.save_settings', side_effect=ValueError('conflict')):
                with self.assertRaisesRegex(ValueError, 'conflict'):
                    retire_legacy_power_setting(path)
            self.assertEqual(before, path.read_bytes())

    def test_active_power_mode_and_broadcast_are_unavailable(self):
        from mcp_local import power_policy, power_windows
        self.assertFalse(hasattr(power_policy, 'PowerPolicyManager'))
        self.assertFalse(hasattr(power_windows, 'ModeChannel'))
        self.assertFalse(hasattr(power_windows, 'follow_mode'))
        manager = power_policy.PowerSchemeManager(MagicMock(), Path('unused'))
        with self.assertRaisesRegex(ValueError, '已移除'):
            manager.apply('extreme')
        with self.assertRaisesRegex(ValueError, '已移除'):
            power_policy.PowerRestoreGuardClient(Path('unused')).apply('extreme')
