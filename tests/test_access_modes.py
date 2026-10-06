"""三模式本機設定、discovery、拒絕 fallback 與隔離 fixture STDIO。"""
import asyncio
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

from access_mode import MODES
from autostart_windows import BackgroundStatus
from connection_cli import set_local_mode
from connection_settings import normalize_connection, load_settings, save_settings
from developer_control import DeveloperControl, backend_status, build_vm_mapping_plan
from local_files_mcp import create_server
from tool_contract import verify_contract
from workspace_reader import WorkspaceReader


class AccessModesTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-access-modes-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.root = self.base / 'root'
        self.root.mkdir()
        self.path = self.base / 'settings.json'
        self.settings = {'settings_version': 7, 'roots': [{'id': 'main', 'path': str(self.root)}],
                         'default_root': 'main', 'tunnel': 'tunnel_fixture'}

    def test_all_modes_save_reload_and_old_versions_cannot_opt_in(self):
        for mode in MODES:
            save_settings({**self.settings, 'access_mode': mode}, self.path)
            self.assertEqual(load_settings(self.path)['access_mode'], mode)
        for version in range(1, 7):
            for mode in ('developer_control', 'host_control'):
                migrated = normalize_connection({**self.settings, 'settings_version': version, 'access_mode': mode})
                self.assertEqual(migrated['access_mode'], 'read_only')
        legacy_full = normalize_connection({**self.settings, 'settings_version': 6, 'access_mode': 'full_control'})
        self.assertEqual(legacy_full['access_mode'], 'read_only')
        self.assertEqual(legacy_full['settings_version'], 8)

    def test_registry_and_contracts_for_all_three_modes(self):
        for mode, count in zip(MODES, (22, 34, 36)):
            workspace = WorkspaceReader(self.settings['roots'], 'main')
            server = create_server(workspace, mode)
            try:
                tools = workspace._registered_tools_provider()
                verify_contract(tools, mode)
                self.assertEqual(len(tools), count)
                diagnostics = workspace.server_diagnostics()
                self.assertTrue(diagnostics['consistent'])
                self.assertEqual(diagnostics['mode_ready'], backend_status()['ready'] if mode == 'developer_control' else True)
                self.assertFalse(any('set_access_mode' == tool.name for tool in tools))
            finally:
                if server._full_control:
                    server._full_control.close()

    def test_cli_changes_only_explicit_fixture_settings(self):
        save_settings(self.settings, self.path)
        completed = subprocess.run([sys.executable, '-B', str(Path(__file__).resolve().parents[1] / 'connection_cli.py'),
            '--settings-file', str(self.path), '--set-access-mode', 'host_control'],
            capture_output=True, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(load_settings(self.path)['access_mode'], 'host_control')
        self.assertFalse((self.base / 'api-key.dpapi').exists())
        self.assertFalse((self.base / 'power-runtime.json').exists())

    def test_cli_verified_background_stops_before_save_then_restarts(self):
        save_settings(self.settings, self.path)
        order = []
        def save(value, path, **kwargs):
            order.append('save')
            save_settings(value, path, **kwargs)
        with patch('connection_cli.CONFIG_FILE', self.path), patch('autostart_windows.get_background_status',
                return_value=BackgroundStatus(True, True, True)), patch('autostart_windows.stop_background',
                side_effect=lambda: order.append('stop')), patch('autostart_windows.start_background',
                side_effect=lambda: order.append('start')), patch('connection_cli.save_settings', side_effect=save):
            set_local_mode(self.path, 'host_control')
        self.assertEqual(order, ['stop', 'save', 'start'])

    def test_cli_refuses_unknown_manual_connection_even_if_saved_mode_matches(self):
        save_settings(self.settings, self.path)
        before = self.path.read_bytes()
        with patch('connection_cli.CONFIG_FILE', self.path), patch('autostart_windows.get_background_status',
                return_value=BackgroundStatus(False, False, False)), patch('connection_cli.existing_tunnel', return_value=True):
            with self.assertRaisesRegex(ValueError, 'MODE_SWITCH_APP_RUNNING'):
                set_local_mode(self.path, 'read_only')
        self.assertEqual(self.path.read_bytes(), before)

    def test_developer_backend_never_falls_back_to_host_or_old_sandbox(self):
        workspace = WorkspaceReader(self.settings['roots'], 'main')
        control = DeveloperControl(workspace)
        self.addCleanup(control.close)
        with patch('developer_control.backend_status', return_value={'ready': False}), patch('host_windows.HostProcess.start') as host, patch('control_sessions.Sandbox') as sandbox:
            with self.assertRaisesRegex(ValueError, 'DEVELOPER_BACKEND_UNAVAILABLE'):
                control.start_process("Write-Output 'must not run'")
        host.assert_not_called()
        sandbox.assert_not_called()
        with patch('developer_control.Path.is_file', return_value=True):
            self.assertTrue(backend_status()['ready'])
            self.assertTrue(backend_status()['execution_adapter_implemented'])
            self.assertTrue(backend_status()['ready'])  # Built-in PowerShell needs no extra toolchain.

    def test_vm_mapping_plan_readonly_tools_and_protected_overlap_denial(self):
        toolchain, state, service = self.base / 'tools & runtime', self.base / 'state', self.base / 'service'
        runtime, windows = self.base / 'python-runtime', self.base / 'windows-system'
        for path in (toolchain, state, service, runtime, windows):
            path.mkdir()
        with patch('developer_control.security_policy.STATE_DIR', state), patch('developer_control.PROJECT', service), patch(
                'developer_control.sys.base_prefix', str(runtime)), patch.dict(os.environ, {'SystemRoot': str(windows)}):
            xml = ET.fromstring(build_vm_mapping_plan([self.root], [toolchain]))
            self.assertEqual([node.text for node in xml.findall('MappedFolders/MappedFolder/ReadOnly')], ['false', 'true'])
            self.assertEqual(xml.findtext('ClipboardRedirection'), 'Disable')
            for roots, tools in (([self.base], []), ([service], []), ([runtime], []), ([windows], []), ([self.root], [state]),
                                 ([self.root], [self.root])):
                with self.assertRaises(ValueError):
                    build_vm_mapping_plan(roots, tools)

    def test_real_stdio_mode_discovery_host_commands_and_downgrade(self):
        from verify_access_modes_stdio import verify
        report = asyncio.run(verify())
        self.assertEqual([row['tools'] for row in report['modes']], [22, 34, 36, 22])
        self.assertEqual(report['developer_vm_acceptance'], 'NOT_RUN_IN_THIS_SCRIPT')
        self.assertEqual(report['modes'][2]['host_parent_and_child_shutdown'], 'PASS')

    def test_vm_mapping_rejects_root_file_alias_to_external_fixture(self):
        external = self.base / 'outside.txt'
        external.write_text('synthetic outside fixture', encoding='utf-8')
        os.link(external, self.root / 'alias.txt')
        with self.assertRaisesRegex(ValueError, 'VM_MAPPING_ALIAS'):
            build_vm_mapping_plan([self.root], [])
        self.assertEqual(external.read_text(encoding='utf-8'), 'synthetic outside fixture')
        toolchain = self.base / 'toolchain'
        toolchain.mkdir()
        control = DeveloperControl(WorkspaceReader(self.settings['roots'], 'main'), toolchains=[str(toolchain)])
        self.addCleanup(control.close)
        with patch('developer_control.backend_status', return_value={'ready': True}), patch(
                'developer_vm.GuardOwner') as guard:
            with self.assertRaisesRegex(ValueError, 'VM_MAPPING_ALIAS'):
                control.start_process('must never execute')
        guard.assert_not_called()
        self.assertEqual(control.list_sessions()['sessions'], [])

    def test_vm_mapping_allows_proven_internal_aliases_but_not_rw_to_ro_aliases(self):
        toolchain = self.base / 'toolchain'
        toolchain.mkdir()
        source = toolchain / 'tool.exe'
        source.write_text('synthetic executable fixture', encoding='utf-8')
        os.link(source, toolchain / 'internal.exe')
        build_vm_mapping_plan([self.root], [toolchain])
        os.link(source, self.root / 'writable-alias.exe')
        with self.assertRaisesRegex(ValueError, 'VM_MAPPING_ALIAS'):
            build_vm_mapping_plan([self.root], [toolchain])

    def test_legacy_toolchains_are_removed_without_granting_maps(self):
        saved = {**self.settings, 'access_mode': 'developer_control',
                 'developer_toolchains': ['../tools', str(self.base)]}
        save_settings(saved, self.path)
        self.assertNotIn('developer_toolchains', load_settings(self.path))
        self.assertNotIn('developer_toolchains', self.path.read_text())
        for version in (6, 7, 8):
            migrated = normalize_connection({**saved, 'settings_version': version, 'access_mode': 'full_control'})
            self.assertEqual(migrated['access_mode'], 'read_only')
            self.assertNotIn('developer_toolchains', migrated)

    def test_removed_mode_rejected_by_all_public_entrypoints(self):
        with self.assertRaises(ValueError):
            create_server(WorkspaceReader(self.settings['roots'], 'main'), 'full_control')
        with self.assertRaises(ValueError):
            set_local_mode(self.path, 'full_control')
        project = Path(__file__).resolve().parents[1]
        for script in ('local_files_mcp.py', 'connection_cli.py'):
            for arguments in (['--access-mode', 'full_control'], ['--developer-toolchain', str(self.base)]):
                result = subprocess.run([sys.executable, '-B', str(project / script), *arguments],
                                        capture_output=True, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
                self.assertEqual(result.returncode, 2, result.stderr)

    def test_empty_detection_can_start_guest_builtin_with_ready_backend(self):
        control = DeveloperControl(WorkspaceReader(self.settings['roots'], 'main'), toolchains=[])
        self.addCleanup(control.close)
        with patch('developer_control.backend_status', return_value={'ready': True}), patch(
                'developer_vm.VMProcess.start', side_effect=ValueError('REACHED_VM_START')) as start:
            with self.assertRaisesRegex(ValueError, 'REACHED_VM_START'):
                control.start_process("Write-Output 'guest builtin'")
        start.assert_called_once()


if __name__ == '__main__':
    unittest.main()
