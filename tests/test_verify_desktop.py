"""桌面驗證包裝器的清理／失敗傳遞回歸；不啟動正式桌面。"""
import contextlib
import io
import json
from pathlib import Path
import stat
import subprocess
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import uuid

from tools.verification import verify_desktop as verifier


class VerifyDesktopTests(unittest.TestCase):
    def setUp(self):
        parent = verifier.ROOT / '.test-tmp'
        parent.mkdir(exist_ok=True)
        self.directory = parent / ('verify-wrapper-' + uuid.uuid4().hex)
        self.directory.mkdir()
        self.output = self.directory / 'run'
        self.states = {}
        self.commands = []

    def tearDown(self):
        verifier.cleanup_fixture(self.directory, self.directory.parent)

    def spawn(self, command, **kwargs):
        self.commands.append(command)
        mode = command[1][2:]
        report = Path(command[command.index('--report') + 1])
        state = self.states.get(mode, 'PASS')
        if state != 'MISSING':
            report.write_text(json.dumps({'state': state}), encoding='utf-8')
        if '--fixture' in command:
            fixture = Path(command[command.index('--fixture') + 1])
            self.assertFalse(list(fixture.iterdir()))
            (fixture / 'synthetic.txt').write_text('fixture', encoding='utf-8')
        process = Mock(pid=123)
        process.__enter__ = Mock(return_value=process)
        process.__exit__ = Mock(return_value=False)
        process.wait.return_value = 0
        return process

    def run_wrapper(self, *extra):
        with patch.object(verifier.subprocess, 'Popen', side_effect=self.spawn), \
                patch.object(verifier, 'wait_for_backend'), contextlib.redirect_stdout(io.StringIO()):
            code = verifier.main(['--no-build', '--executable', str(self.directory / 'synthetic.exe'),
                                  '--output-dir', str(self.output), *extra])
        return code, json.loads((self.output / 'summary.json').read_text(encoding='utf-8'))

    def test_self_test_needs_no_fixture_and_integration_cleans_owned_directory(self):
        code, summary = self.run_wrapper()
        self.assertEqual((code, summary['state']), (0, 'PASS'))
        self.assertNotIn('--fixture', self.commands[0])
        self.assertEqual(summary['results'][0]['cleanup'], 'NOT_REQUIRED')
        self.assertEqual(summary['results'][1]['cleanup'], 'PASS')
        self.assertFalse((self.output / 'integration-test-fixture').exists())

    def test_permission_failure_is_retained_nonzero_without_retry_or_chmod(self):
        original = Path.unlink
        denied = []

        def unlink(path, *args, **kwargs):
            if path.name == 'synthetic.txt':
                denied.append(path)
                raise PermissionError(5, 'simulated WinError 5', str(path))
            return original(path, *args, **kwargs)

        with patch.object(Path, 'unlink', unlink), patch('os.chmod') as chmod:
            code, summary = self.run_wrapper('--benchmark')
        self.assertEqual((code, summary['state']), (1, 'FAIL'))
        self.assertEqual(summary['results'][-1]['cleanup'], 'BLOCKED')
        self.assertEqual(len(denied), 1)
        self.assertEqual(len(self.commands), 2)
        self.assertTrue(denied[0].exists())
        chmod.assert_not_called()

    def test_missing_report_with_zero_exit_fails(self):
        self.states['self-test'] = 'MISSING'
        code, summary = self.run_wrapper()
        self.assertEqual((code, summary['results'][0]['state']), (1, 'NO_REPORT'))
        self.assertEqual(len(self.commands), 1)

    def test_failed_report_with_zero_exit_fails(self):
        self.states['self-test'] = 'FAIL'
        self.assertEqual(self.run_wrapper()[0], 1)

    def test_unconfirmed_backend_exit_retains_fixture(self):
        with patch.object(verifier.subprocess, 'Popen', side_effect=self.spawn), \
                patch.object(verifier, 'wait_for_backend', side_effect=RuntimeError('still active')), \
                contextlib.redirect_stdout(io.StringIO()):
            code = verifier.main(['--no-build', '--output-dir', str(self.output),
                                  '--executable', str(self.directory / 'synthetic.exe')])
        summary = json.loads((self.output / 'summary.json').read_text(encoding='utf-8'))
        self.assertEqual(code, 1)
        self.assertEqual(summary['results'][-1]['cleanup'], 'RETAINED_UNTIL_EXIT_CONFIRMED')
        self.assertTrue((self.output / 'integration-test-fixture/synthetic.txt').exists())

    def test_build_output_and_intermediates_are_isolated(self):
        with patch.object(verifier.subprocess, 'run', return_value=SimpleNamespace(returncode=0, stdout=b'', stderr=b'')) as build, \
                patch.object(verifier.subprocess, 'Popen', side_effect=self.spawn), \
                patch.object(verifier, 'wait_for_backend'), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(verifier.main(['--output-dir', str(self.output)]), 0)
        command = build.call_args.args[0]
        self.assertEqual(command[command.index('-OutputDirectory') + 1], str(self.output / 'app'))
        self.assertEqual(command[command.index('-IntermediateDirectory') + 1], str(self.output / 'obj'))
        self.assertEqual(self.commands[0][0], str(self.output / 'app/McpLocal.Desktop.exe'))

    def test_build_failure_still_writes_failure_summary(self):
        with patch.object(verifier.subprocess, 'run', return_value=SimpleNamespace(returncode=1, stdout=b'', stderr=b'failure')), \
                patch.object(verifier.subprocess, 'Popen') as spawn, contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(verifier.main(['--output-dir', str(self.output)]), 1)
        spawn.assert_not_called()
        self.assertEqual(json.loads((self.output / 'summary.json').read_text())['state'], 'FAIL')

    def test_existing_output_is_rejected_without_overwrite(self):
        self.output.mkdir()
        sentinel = self.output / 'evidence.txt'
        sentinel.write_text('retain', encoding='utf-8')
        with self.assertRaises(FileExistsError), patch.object(verifier.subprocess, 'Popen') as spawn:
            verifier.main(['--no-build', '--output-dir', str(self.output)])
        spawn.assert_not_called()
        self.assertEqual(sentinel.read_text(), 'retain')

    def test_cleanup_refuses_outside_directory(self):
        with self.assertRaisesRegex(RuntimeError, 'escaped'):
            verifier.cleanup_fixture(self.directory, self.output)
        self.assertTrue(self.directory.exists())

    def test_cleanup_preflights_reparse_points_before_any_delete(self):
        nested = self.directory / 'nested'
        nested.mkdir()
        regular = nested / 'a.txt'
        regular.write_text('retain', encoding='utf-8')
        link = nested / 'z.txt'
        link.write_text('simulated reparse point', encoding='utf-8')
        original = Path.lstat

        def lstat(path, *args, **kwargs):
            if path == link:
                return SimpleNamespace(st_mode=stat.S_IFREG, st_file_attributes=stat.FILE_ATTRIBUTE_REPARSE_POINT)
            return original(path, *args, **kwargs)

        with patch.object(Path, 'lstat', lstat), self.assertRaisesRegex(RuntimeError, 'reparse'):
            verifier.cleanup_fixture(nested, self.directory)
        self.assertTrue(regular.exists())
        self.assertTrue(link.exists())

    def test_backend_parent_may_be_python_launcher_and_is_waited_without_termination(self):
        fixture = self.directory / 'fixture'
        fixture.mkdir()
        (fixture / 'processes.json').write_text(json.dumps({'frontend': 222, 'backend': 333}))
        kernel = Mock()
        kernel.OpenProcess.return_value = 10
        kernel.WaitForSingleObject.return_value = 0
        with patch.object(verifier.ctypes, 'WinDLL', return_value=kernel):
            verifier.wait_for_backend(fixture, 111, self.directory)
        self.assertEqual({call.args[2] for call in kernel.OpenProcess.call_args_list}, {222, 333})
        self.assertTrue(all(call.args[:2] == (0x100000, False) for call in kernel.OpenProcess.call_args_list))
        kernel.TerminateProcess.assert_not_called()
        evidence = json.loads((self.directory / 'fixture-processes.json').read_text())
        self.assertEqual(evidence, {'frontend': 111, 'backend': 333, 'backend_parent': 222, 'state': 'EXITED'})

    def test_backend_wait_timeout_does_not_claim_exit(self):
        fixture = self.directory / 'fixture'
        fixture.mkdir()
        (fixture / 'processes.json').write_text(json.dumps({'frontend': 222, 'backend': 333}))
        kernel = Mock()
        kernel.OpenProcess.return_value = 10
        kernel.WaitForSingleObject.return_value = 258
        with patch.object(verifier.ctypes, 'WinDLL', return_value=kernel), self.assertRaisesRegex(RuntimeError, 'not confirmed'):
            verifier.wait_for_backend(fixture, 111, self.directory)
        self.assertFalse((self.directory / 'fixture-processes.json').exists())
        kernel.CloseHandle.assert_called_once_with(10)
        kernel.TerminateProcess.assert_not_called()


if __name__ == '__main__':
    unittest.main()
