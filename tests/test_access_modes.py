"""三模式本機設定、discovery、拒絕 fallback 與隔離 fixture STDIO。"""
import asyncio
from contextlib import contextmanager, ExitStack
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch, Mock
import xml.etree.ElementTree as ET

from mcp_local.access_mode import HOST_CONTROL, MODES
from mcp_local import security_policy
from mcp_local.autostart_windows import BackgroundStatus
from mcp_local.connection_cli import set_local_mode
from mcp_local.connection_settings import normalize_connection, load_settings, save_settings
from mcp_local.developer_control import DeveloperControl, backend_status, build_vm_mapping_plan
from mcp_local.control_sessions import Session
from mcp_local.host_files import HostFiles
from mcp_local.local_files_mcp import create_server
from mcp_local.tool_contract import verify_contract
from mcp_local.workspace_reader import WorkspaceReader
from tools.diagnostics.remote_smoke_test import verify_tools


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

    def test_remote_smoke_requires_complete_mode_contract(self):
        for mode, count in (('read_only', 22), ('host_control', 36)):
            workspace = WorkspaceReader(self.settings['roots'], 'main')
            server = create_server(workspace, mode)
            try:
                tools = workspace._registered_tools_provider()
                self.assertEqual(len(tools), count)
                verify_tools(tools, mode)
                for invalid in (tools[:16], tools[:-1], tools + [tools[0]], tools[:-1] + [tools[0]]):
                    with self.subTest(mode=mode, count=len(invalid)), self.assertRaises(ValueError):
                        verify_tools(invalid, mode)
                for field in ('name', 'inputSchema', 'readOnlyHint', 'destructiveHint', 'openWorldHint', 'idempotentHint'):
                    changed = [tool.model_copy(deep=True) for tool in tools]
                    tool = next(tool for tool in changed if tool.name == ('write_file' if mode == 'host_control' else 'read_file'))
                    if field == 'name':
                        tool.name = 'unexpected_tool'
                    elif field == 'inputSchema':
                        tool.inputSchema['properties']['unexpected'] = {'type': 'string'}
                    else:
                        setattr(tool.annotations, field, not getattr(tool.annotations, field))
                    with self.subTest(mode=mode, field=field), self.assertRaises(ValueError):
                        verify_tools(changed, mode)
                changed = [tool.model_copy(deep=True) for tool in tools]
                changed[0].annotations = None
                with self.assertRaises(ValueError):
                    verify_tools(changed, mode)
                with self.assertRaises(ValueError):
                    verify_tools(tools, 'host_control' if mode == 'read_only' else 'read_only')
            finally:
                if server._full_control:
                    server._full_control.close()
        with self.assertRaises(ValueError):
            verify_tools([], 'developer_control')

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
        with patch('mcp_local.connection_cli.CONFIG_FILE', self.path), patch('mcp_local.autostart_windows.get_background_status',
                return_value=BackgroundStatus(True, True, True)), patch('mcp_local.autostart_windows.stop_background',
                side_effect=lambda: order.append('stop')), patch('mcp_local.autostart_windows.start_background',
                side_effect=lambda: order.append('start')), patch('mcp_local.connection_cli.save_settings', side_effect=save):
            set_local_mode(self.path, 'host_control')
        self.assertEqual(order, ['stop', 'save', 'start'])

    def test_cli_refuses_unknown_manual_connection_even_if_saved_mode_matches(self):
        save_settings(self.settings, self.path)
        before = self.path.read_bytes()
        with patch('mcp_local.connection_cli.CONFIG_FILE', self.path), patch('mcp_local.autostart_windows.get_background_status',
                return_value=BackgroundStatus(False, False, False)), patch('mcp_local.connection_cli.existing_tunnel', return_value=True):
            with self.assertRaisesRegex(ValueError, 'MODE_SWITCH_APP_RUNNING'):
                set_local_mode(self.path, 'read_only')
        self.assertEqual(self.path.read_bytes(), before)

    def test_developer_backend_never_falls_back_to_host_or_old_sandbox(self):
        workspace = WorkspaceReader(self.settings['roots'], 'main')
        control = DeveloperControl(workspace)
        self.addCleanup(control.close)
        with patch('mcp_local.developer_control.backend_status', return_value={'ready': False}), patch('mcp_local.host_windows.HostProcess.start') as host, patch('mcp_local.control_sessions.Sandbox') as sandbox:
            with self.assertRaisesRegex(ValueError, 'DEVELOPER_BACKEND_UNAVAILABLE'):
                control.start_process("Write-Output 'must not run'")
        host.assert_not_called()
        sandbox.assert_not_called()
        with patch('mcp_local.developer_control.Path.is_file', return_value=True):
            self.assertTrue(backend_status()['ready'])
            self.assertTrue(backend_status()['execution_adapter_implemented'])
            self.assertTrue(backend_status()['ready'])  # Built-in PowerShell needs no extra toolchain.

    def test_vm_mapping_plan_readonly_tools_and_protected_overlap_denial(self):
        toolchain, state, service = self.base / 'tools & runtime', self.base / 'state', self.base / 'service'
        runtime, windows = self.base / 'python-runtime', self.base / 'windows-system'
        for path in (toolchain, state, service, runtime, windows):
            path.mkdir()
        with patch('mcp_local.developer_control.security_policy.STATE_DIR', state), patch('mcp_local.developer_control.PROJECT', service), patch(
                'mcp_local.security_policy.sys.base_prefix', str(runtime)), patch.dict(os.environ, {'SystemRoot': str(windows)}):
            xml = ET.fromstring(build_vm_mapping_plan([self.root], [toolchain]))
            self.assertEqual([node.text for node in xml.findall('MappedFolders/MappedFolder/ReadOnly')], ['false', 'true'])
            self.assertEqual(xml.findtext('ClipboardRedirection'), 'Disable')
            for roots, tools in (([self.base], []), ([service], []), ([runtime], []), ([windows], []), ([self.root], [state]),
                                 ([self.root], [self.root])):
                with self.assertRaises(ValueError):
                    build_vm_mapping_plan(roots, tools)

    def test_real_stdio_mode_discovery_host_commands_and_downgrade(self):
        from tools.verification.verify_access_modes_stdio import verify
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
        with patch('mcp_local.developer_control.backend_status', return_value={'ready': True}), patch(
                'mcp_local.developer_vm.GuardOwner') as guard:
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
        with patch('mcp_local.developer_control.backend_status', return_value={'ready': True}), patch(
                'mcp_local.developer_vm.VMProcess.start', side_effect=ValueError('REACHED_VM_START')) as start:
            with self.assertRaisesRegex(ValueError, 'REACHED_VM_START'):
                control.start_process("Write-Output 'guest builtin'")
        start.assert_called_once()


class PythonRuntimeProtectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-runtime-policy-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        self.control = DeveloperControl(self.workspace, toolchains=[])
        self.addCleanup(self.control.close)

    @contextmanager
    def runtime_fixture(self):
        prefixes = ('prefix', 'exec_prefix', 'base_prefix', 'base_exec_prefix')
        directories = {name: self.root / name for name in (*prefixes, 'executable', 'base_executable', 'imports')}
        for directory in directories.values():
            codec = directory / 'Lib/encodings/cp437.py'
            codec.parent.mkdir(parents=True)
            codec.write_bytes(b'# synthetic codec only\n')
        archive = self.root / 'python-import.zip'
        archive.write_bytes(b'synthetic import archive')
        missing_archive = self.root / 'future-import.zip'
        with ExitStack() as stack:
            for name in prefixes:
                stack.enter_context(patch.object(sys, name, str(directories[name])))
            for name in ('executable', '_base_executable'):
                stack.enter_context(patch.object(sys, name, str(directories[name.lstrip('_')] / 'python.exe'), create=True))
            stack.enter_context(patch.object(sys, 'path', [*sys.path, str(directories['imports']), str(archive), str(missing_archive)]))
            yield directories, archive, missing_archive

    def test_developer_mutations_reject_all_active_python_roots(self):
        with self.runtime_fixture() as (directories, _, _):
            safe = self.control.write_file('safe.py', '# safe sibling\n')
            with patch.object(self.control.files, 'commit') as commit:
                for directory in directories.values():
                    target = directory / 'Lib/encodings/cp437.py'
                    relative = target.relative_to(self.root).as_posix()
                    original = target.read_bytes()
                    sha256 = hashlib.sha256(original).hexdigest()
                    calls = [
                        lambda: self.control.write_file(relative, 'changed', 'rewrite', sha256),
                        lambda: self.control.write_file(relative, 'changed', 'append', sha256),
                        lambda: self.control.edit_block(relative, 'synthetic', 'changed', sha256),
                        lambda: self.control.write_file(relative + '.py', 'created'),
                        lambda: self.control.create_directory(directory.name + '/new'),
                        lambda: self.control.delete_file(relative, sha256),
                        lambda: self.control.move_file(relative, 'moved.py', sha256),
                        lambda: self.control.move_file('safe.py', relative + '.py', safe['sha256']),
                    ]
                    for index, call in enumerate(calls):
                        with self.subTest(root=directory.name, operation=index), self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                            call()
                        self.assertEqual(target.read_bytes(), original)
                    self.assertIn('synthetic', self.workspace.reader(None).read_file(relative)['content'])
                    self.assertFalse(Path(str(target) + '.py').exists())
                    self.assertFalse((directory / 'new').exists())
                commit.assert_not_called()
            changed = self.control.edit_block('safe.py', 'safe', 'ordinary', safe['sha256'])
            self.control.move_file('safe.py', 'ordinary.py', changed['sha256'])
            self.control.delete_file('ordinary.py', changed['sha256'])
            self.assertFalse((self.root / 'moved.py').exists())

    def test_import_archives_ancestors_and_canonical_aliases_are_protected(self):
        with self.runtime_fixture() as (_, archive, missing_archive):
            original = archive.read_bytes()
            sha256 = hashlib.sha256(original).hexdigest()
            calls = [lambda: self.control.delete_file(archive.name, sha256),
                     lambda: self.control.move_file(archive.name, 'moved.zip', sha256),
                     lambda: self.control.write_file(missing_archive.name, 'created')]
            for index, call in enumerate(calls):
                with self.subTest(operation=index), self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                    call()
            self.assertEqual(archive.read_bytes(), original)
            self.assertFalse(missing_archive.exists())
            self.assertTrue(security_policy.overlaps_python_runtime(self.root))
            self.assertFalse(security_policy.overlaps_python_runtime(self.root / 'python-import.zip-neighbor'))
            alias = self.root / 'PYTHON~1.ZIP'
            resolve = Path.resolve
            def canonical(path, *args, **kwargs):
                return archive if path == alias else resolve(path, *args, **kwargs)
            with patch.object(Path, 'resolve', autospec=True, side_effect=canonical):
                self.assertTrue(security_policy.overlaps_python_runtime(alias))

    def test_export_and_host_file_api_share_runtime_guard(self):
        with self.runtime_fixture() as (directories, _, _):
            target = directories['base_prefix'] / 'Lib/encodings/cp437.py'
            relative = target.relative_to(self.root).as_posix()
            original = target.read_bytes()
            sha256 = hashlib.sha256(original).hexdigest()
            spool = self.root / 'spool'
            spool.mkdir()
            (spool / 'result.py').write_text('# exported fixture', encoding='utf-8')
            session = Session(Mock(workspace=spool, pid=1))
            session.done.set()
            self.control.sessions.sessions[session.identifier] = session
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                self.control.export_session_file(session.identifier, 'result.py', relative, sha256)
            host = HostFiles(self.workspace, HOST_CONTROL)
            self.addCleanup(host.close)
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                host.write(str(target), 'changed', 'rewrite', sha256, None)
            safe = host.write(str(self.root / 'host-safe.py'), '# safe', 'create', None, None)
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                host.move(str(self.root / 'host-safe.py'), str(target) + '.py', safe['sha256'], None)
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                host.move(str(target), str(self.root / 'host-moved.py'), sha256, None)
            self.assertEqual(target.read_bytes(), original)
            self.assertEqual((self.root / 'host-safe.py').read_text(), '# safe')

    def test_vm_writable_maps_use_same_runtime_paths_and_allow_readonly_tools(self):
        with self.runtime_fixture() as (directories, _, _):
            for directory in directories.values():
                for root in (directory, directory / 'Lib', self.root):
                    with self.subTest(root=root), self.assertRaisesRegex(ValueError, 'VM_MAPPING_PROTECTED'):
                        build_vm_mapping_plan([root], [])
            safe = self.root / 'safe-project'
            safe.mkdir()
            xml = ET.fromstring(build_vm_mapping_plan([safe], [directories['base_prefix']]))
            self.assertEqual([item.text for item in xml.findall('MappedFolders/MappedFolder/ReadOnly')], ['false', 'true'])

    def test_relative_import_paths_and_optional_base_executable(self):
        with self.runtime_fixture(), patch.object(sys, 'path', ['', 'relative-import', None]), patch.object(sys, '_base_executable', None):
            paths = security_policy.python_runtime_paths()
            self.assertIn(Path.cwd().resolve(), paths)
            self.assertIn((Path.cwd() / 'relative-import').resolve(), paths)

    def test_disabled_user_site_is_protected_for_base_python_children(self):
        user_site = self.root / 'user-site'
        user_site.mkdir()
        target = user_site / 'usercustomize.py'
        relative = target.relative_to(self.root).as_posix()
        with self.runtime_fixture(), patch('mcp_local.security_policy.site.ENABLE_USER_SITE', False), patch(
                'mcp_local.security_policy.site.getusersitepackages', return_value=str(user_site)):
            self.assertNotIn(str(user_site), sys.path)
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                self.control.write_file(relative, '# synthetic startup hook')
            self.assertFalse(target.exists())
            original = b'# existing synthetic hook\n'
            target.write_bytes(original)
            with self.assertRaisesRegex(ValueError, 'CONTROL_PLANE_PROTECTED'):
                self.control.write_file(relative, '# changed', 'rewrite', hashlib.sha256(original).hexdigest())
            self.assertEqual(target.read_bytes(), original)
            self.assertIn('synthetic', self.workspace.reader(None).read_file(relative)['content'])
            with self.assertRaisesRegex(ValueError, 'VM_MAPPING_PROTECTED'):
                build_vm_mapping_plan([user_site], [])


if __name__ == '__main__':
    unittest.main()
