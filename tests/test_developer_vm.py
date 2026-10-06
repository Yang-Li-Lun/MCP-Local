"""VM guard、來源別名與 guest 回傳檔防護；此單元套件不啟動真實 VM。"""
import json
import os
from pathlib import Path
import subprocess
import stat
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch
import uuid

from connection_runtime import Connection, ConnectionErrorKind
from control_files import ControlledFiles
from developer_vm import VMProcess, remove_owned_tree
from developer_vm_guard import Lifecycle, find_cli
from developer_vm_client import guard_environment
from workspace_reader import WorkspaceReader


class DeveloperVMTests(unittest.TestCase):
    def test_stop_only_owned_id_and_requires_absence_proof(self):
        lifecycle = Lifecycle(Path('fixture-wsb.exe'))
        own, foreign = str(uuid.uuid4()), str(uuid.uuid4())
        lifecycle.identifier = own
        with patch.object(lifecycle, 'ids', side_effect=[{own, foreign}, {foreign}]), patch(
                'developer_vm_guard.invoke_cli') as invoke:
            lifecycle.stop()
        invoke.assert_called_once_with(lifecycle.executable, 'stop', '--id', own)
        self.assertIsNone(lifecycle.identifier)
        lifecycle.identifier = own
        with patch.object(lifecycle, 'ids', return_value={own}), patch('developer_vm_guard.invoke_cli'):
            with self.assertRaisesRegex(ValueError, 'VM_STOP_UNCONFIRMED'):
                lifecycle.stop()
        self.assertEqual(lifecycle.identifier, own)

    def test_start_refuses_foreign_vm_without_mutation(self):
        lifecycle = Lifecycle(Path('fixture-wsb.exe'))
        with patch.object(lifecycle, 'ids', return_value={str(uuid.uuid4())}), patch(
                'developer_vm_guard.invoke_cli') as invoke:
            with self.assertRaisesRegex(ValueError, 'VM_BUSY'):
                lifecycle.start('<Configuration/>', threading.Event())
        invoke.assert_not_called()

    def test_cli_resolution_uses_registered_package_not_programfiles_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            package = base / 'WindowsApps' / 'fixture-package'
            package.mkdir(parents=True)
            (package / 'wsb.exe').write_bytes(b'not executable fixture')
            output = json.dumps({'PackageFamilyName': 'MicrosoftWindows.WindowsSandbox_cw5n1h2txyewy',
                                 'InstallLocation': str(package), 'ProgramFiles': str(base)}).encode()
            completed = subprocess.CompletedProcess([], 0, stdout=output, stderr=b'')
            with patch.dict(os.environ, {'ProgramFiles': 'Z:\\incorrect'}), patch(
                    'developer_vm_guard.subprocess.run', return_value=completed):
                self.assertEqual(find_cli(), package / 'wsb.exe')
        with patch.dict(os.environ, {'PATH': 'C:\\untrusted-root', 'PSModulePath': 'C:\\untrusted-modules',
                                     'PYTHONPATH': 'C:\\untrusted-python', 'CONTROL_PLANE_API_KEY': 'synthetic'}):
            environment = guard_environment()
        self.assertNotIn('untrusted', environment['PATH'])
        self.assertNotIn('untrusted', environment['PSMODULEPATH'])
        self.assertNotIn('PYTHONPATH', environment)
        self.assertNotIn('CONTROL_PLANE_API_KEY', environment)

    def test_guard_is_created_before_job_and_closed_before_done(self):
        events = []
        guard = Mock(encode=Mock(return_value='synthetic-guard-configuration'))
        guard.close.side_effect = lambda: events.append('guard-close')
        job = Mock()
        job.close.side_effect = lambda: events.append('job-close')
        def create_guard():
            events.append('guard-create')
            return guard
        def create_job():
            events.append('job-create')
            return job
        connection = Connection()
        with patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex), patch(
                'connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=[]), patch(
                'developer_vm_client.GuardOwner', side_effect=create_guard), patch('tray_windows.Job', side_effect=create_job):
            connection.start({'settings_version': 7, 'access_mode': 'developer_control',
                              'developer_toolchains': []}, 'synthetic-key')
            connection.thread.join(5)
        self.assertFalse(connection.thread.is_alive())
        self.assertEqual(events, ['guard-create', 'job-create', 'job-close', 'guard-close'])
        self.assertEqual(list(connection.events.queue)[-1], ('done', '已停止'))

    def test_guard_cleanup_failure_blocks_relaunch(self):
        guard = Mock(encode=Mock(return_value='synthetic'))
        guard.close.side_effect = ValueError('VM_STOP_UNCONFIRMED')
        connection = Connection()
        with patch('connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid.uuid4().hex), patch(
                'connection_runtime.existing_tunnel', return_value=False), patch(
                'connection_runtime.build_commands', return_value=[]), patch(
                'developer_vm_client.GuardOwner', return_value=guard), patch('tray_windows.Job'):
            connection.start({'settings_version': 7, 'access_mode': 'developer_control',
                              'developer_toolchains': []}, 'synthetic-key')
            connection.thread.join(5)
        self.assertTrue(connection.cleanup_failed)
        self.assertEqual(connection.error_kind, ConnectionErrorKind.RESOURCE_CLEANUP_FAILED)
        with self.assertRaisesRegex(ValueError, 'RESOURCE_CLEANUP_FAILED'):
            connection.start({}, '')

    def test_guest_output_hardlink_is_rejected_before_reading_external_bytes(self):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary).resolve()
            root = base / 'root'
            root.mkdir()
            workspace = WorkspaceReader([{'id': 'main', 'path': str(root)}], 'main')
            files = ControlledFiles(workspace, 'full_control')
            process = VMProcess(files, 'main', None, [], 60)
            process.spool.mkdir()
            from local_files_mcp import FileReader
            process.spool_reader = FileReader(process.spool)
            external = base / 'synthetic-private.txt'
            external.write_text('MUST_NOT_READ', encoding='utf-8')
            os.link(external, process.spool / 'output.txt')
            with self.assertRaises(ValueError):
                process.collect_output()
            self.assertTrue(process.stdout.queue.empty())
            process.close()
            self.assertEqual(external.read_text(encoding='utf-8'), 'MUST_NOT_READ')
            owned = root / 'owned-cleanup'
            owned.mkdir()
            safe = owned / 'readonly.txt'
            safe.write_text('fixture', encoding='utf-8')
            safe.chmod(stat.S_IREAD)
            remove_owned_tree(owned)
            self.assertFalse(owned.exists())
            owned.mkdir()
            os.link(external, owned / 'linked.txt')
            external.chmod(stat.S_IREAD)
            try:
                with self.assertRaises(PermissionError):
                    remove_owned_tree(owned)
                self.assertTrue(external.lstat().st_file_attributes & 1)
            finally:
                external.chmod(stat.S_IREAD | stat.S_IWRITE)


if __name__ == '__main__':
    unittest.main()
