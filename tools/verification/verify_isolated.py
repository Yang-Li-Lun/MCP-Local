"""預設完整隔離回歸；禁止測試開啟正式狀態檔或執行正式通道與排程命令。"""

# Direct script execution and python -m use the same installed package.
if __package__ in (None, ""):
    import sys
    from pathlib import Path
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import ast
import argparse
from contextlib import ExitStack
from datetime import datetime
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch


NATIVE_TESTS = {
    'test_full_control.FullControlTests.test_actual_appcontainer_interaction_isolation_and_cleanup',
    'test_full_control.FullControlTests.test_lifetime_and_terminate_own_job',
    'test_full_control_stdio.FullControlStdioTests.test_real_stdio_switches_and_calls',
}


def verify_user_token() -> None:
    """Reuse the host-mode token check without starting a process or changing privileges."""
    if os.name == 'nt':
        from mcp_local.host_windows import HostProcess
        probe = HostProcess(Path.cwd())
        probe.close()


def check_test_environment() -> dict:
    """Reject unsuitable test environments before their failures obscure regressions."""
    from mcp_local.security_policy import STATE_DIR, overlaps_python_runtime
    try:
        verify_user_token()
    except (OSError, ValueError) as error:
        raise RuntimeError('TEST_TOKEN_UNSUITABLE：請在一般未提升的 Windows 使用者環境執行；'
                           '受限 AppContainer 或管理員 token 不符合主機模式回歸前置條件。') from error
    temporary = Path(tempfile.gettempdir()).resolve()
    state = STATE_DIR.resolve()
    if overlaps_python_runtime(temporary) or temporary.is_relative_to(state) or state.is_relative_to(temporary):
        raise RuntimeError('TEST_TEMP_PROTECTED：TEMP 必須位於專案／Python 匯入與執行路徑、正式狀態目錄之外；'
                           '請使用一般使用者的系統 TEMP，不要為通過測試放寬服務保護。')
    try:
        with tempfile.TemporaryDirectory(prefix='mcp-regression-probe-', dir=temporary) as directory:
            nested = Path(directory) / 'nested'
            nested.mkdir()
            source, link = nested / 'source.txt', nested / 'link.txt'
            source.write_text('synthetic regression probe', encoding='utf-8')
            os.link(source, link)
            if source.stat().st_nlink < 2 or source.read_bytes() != link.read_bytes():
                raise OSError('hardlink probe failed')
    except OSError as error:
        raise RuntimeError('TEST_TEMP_UNUSABLE：測試 TEMP 無法完成子目錄寫入、硬連結或清理；'
                           '請使用可寫入且支援硬連結的系統 TEMP。') from error
    return {'token': 'NORMAL_UNELEVATED' if os.name == 'nt' else 'NOT_WINDOWS',
            'temporary_directory': str(temporary), 'nested_write_hardlink_cleanup': 'PASS'}


def offline_suite(suite):
    """只跳過明確列出的原生 AppContainer／STDIO 測試，其餘照常執行。"""
    for test in suite:
        if isinstance(test, unittest.TestSuite):
            yield from offline_suite(test)
        else:
            if test.id() in NATIVE_TESTS:
                method = getattr(test, test._testMethodName)
                setattr(test, test._testMethodName, unittest.skip('offline: no AppContainer profile changes')(method))
            yield test


def main():
    root = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--offline', action='store_true', help='停用 AppContainer/VM 原生啟動，保留 mock 與合成主機回歸')
    parser.add_argument('--tests', nargs='+', help='只復測指定 unittest ID 或模組')
    parser.add_argument('--output-dir', type=Path,
                        default=root / 'reports/validation' / datetime.now().strftime('isolated-%Y%m%d-%H%M%S-%f'))
    args = parser.parse_args()
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    os.chdir(root)
    state = (Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local').resolve()
    def guard(event, arguments):
        if event == 'mcp.power.mutate':
            raise RuntimeError('ISOLATION_GUARD：禁止測試修改真實 Windows 電源狀態。')
        if event == 'open' and isinstance(arguments[0], (str, bytes, os.PathLike)):
            path = Path(os.fsdecode(arguments[0])).absolute()
            if path.is_relative_to(state):
                raise RuntimeError('ISOLATION_GUARD：禁止測試讀寫正式狀態檔。')
        if event == 'subprocess.Popen':
            executable = Path(str(arguments[0])).name.casefold()
            if executable in ('schtasks.exe', 'tunnel-client.exe', 'cloudflared.exe', 'windowssandbox.exe', 'wsb.exe'):
                raise RuntimeError('ISOLATION_GUARD：禁止正式排程或通道命令。')
    sys.addaudithook(guard)
    try:
        environment = check_test_environment()
    except RuntimeError as error:
        report = {'state': 'ENVIRONMENT_NOT_READY', 'tests': 0, 'error': str(error),
                  'production': 'NOT_OPERATED'}
        (output / 'engineering-results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
        print(json.dumps(report))
        return 2
    sources = sorted(list(root.glob('*.py')) + [path for folder in ('mcp_local', 'tools', 'tests')
                                              for path in (root / folder).rglob('*.py')])
    for path in sources:
        ast.parse(path.read_text(encoding='utf-8-sig'), filename=path.name)
    dependencies = subprocess.run([sys.executable, '-B', '-m', 'pip', 'check'], capture_output=True, text=True)
    started = time.monotonic()
    sys.path.insert(0, str(root / 'tests'))
    suite = (unittest.defaultTestLoader.loadTestsFromNames(args.tests) if args.tests
             else unittest.defaultTestLoader.discover('tests'))
    with ExitStack() as stack:
        if args.offline:
            suite = unittest.TestSuite(offline_suite(suite))
            stack.enter_context(patch('mcp_local.sandbox_windows.Sandbox.__init__', side_effect=RuntimeError('OFFLINE_NATIVE_BLOCKED')))
            stack.enter_context(patch('mcp_local.developer_vm_client.GuardOwner.__init__', side_effect=RuntimeError('OFFLINE_VM_BLOCKED')))
        with (output / 'engineering-tests.txt').open('x', encoding='utf-8') as log:
            result = unittest.TextTestRunner(stream=log, verbosity=2).run(suite)
    report = {'state': 'PASS' if result.wasSuccessful() and dependencies.returncode == 0 else 'FAIL',
              'environment': environment,
              'python': sys.version.split()[0], 'mcp': importlib.metadata.version('mcp'),
              'pydantic': importlib.metadata.version('pydantic'), 'syntax_files': len(sources),
              'pip_check': dependencies.returncode, 'tests': result.testsRun,
              'failures': len(result.failures), 'errors': len(result.errors),
              'skipped': len(result.skipped), 'elapsed_seconds': round(time.monotonic()-started, 3),
              'remote_acceptance': 'NOT_RUN', 'production_scheduler': 'NOT_RUN', 'system_power_changes': 'BLOCKED_BY_AUDIT_GUARD',
              'offline': args.offline, 'skipped_tests': [(test.id(), reason) for test, reason in result.skipped],
              'isolation': 'audit guard for main test process; subprocess fixtures separately reviewed'}
    (output / 'engineering-results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(report))
    return 0 if result.wasSuccessful() and dependencies.returncode == 0 else 1


if __name__ == '__main__':
    raise SystemExit(main())
