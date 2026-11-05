"""實際 WPF STA 控制項與 Python 私有管線整合；只使用新建的隔離 fixture。"""
from __future__ import annotations
import argparse
import ctypes
import json
from pathlib import Path
import stat
import subprocess

ROOT = Path(__file__).resolve().parents[2]


def wait_for_backend(fixture: Path, frontend_pid: int, output: Path) -> None:
    """只等待本次 fixture 記錄的程序，不終止或操作其他程序。"""
    marker = json.loads((fixture / 'processes.json').read_text(encoding='utf-8'))
    if any(type(marker.get(name)) is not int or marker[name] <= 0 for name in ('frontend', 'backend')):
        raise RuntimeError('Invalid fixture process record')
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes = [ctypes.c_ulong, ctypes.c_int, ctypes.c_ulong]
    kernel.OpenProcess.restype = ctypes.c_void_p
    kernel.WaitForSingleObject.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
    kernel.WaitForSingleObject.restype = ctypes.c_ulong
    kernel.CloseHandle.argtypes = [ctypes.c_void_p]
    kernel.CloseHandle.restype = ctypes.c_int
    # The fixture's historical "frontend" field is os.getppid(), which can be
    # the Python venv launcher. The actual WPF PID comes from our Popen handle.
    for pid in {marker['frontend'], marker['backend']}:
        handle = kernel.OpenProcess(0x100000, False, pid)  # SYNCHRONIZE only
        if not handle:
            error = ctypes.get_last_error()
            if error != 87:  # ERROR_INVALID_PARAMETER: PID already exited
                raise ctypes.WinError(error)
        else:
            try:
                if kernel.WaitForSingleObject(handle, 10000) != 0:
                    raise RuntimeError('Fixture backend/parent exit not confirmed; fixture retained')
            finally:
                kernel.CloseHandle(handle)
    evidence = {'frontend': frontend_pid, 'backend': marker['backend'],
                'backend_parent': marker['frontend'], 'state': 'EXITED'}
    (output / (fixture.name + '-processes.json')).write_text(json.dumps(evidence, indent=2), encoding='utf-8')


def cleanup_fixture(fixture: Path, output: Path) -> int:
    """只清理本次新建目錄；先盤點，不跟隨連結、不調整權限、不重試。"""
    if fixture.parent != output or fixture.resolve() != fixture:
        raise RuntimeError('Fixture cleanup escaped output directory')
    entries = []

    def inventory(path: Path) -> None:
        info = path.lstat()
        if (stat.S_ISLNK(info.st_mode) or
                getattr(info, 'st_file_attributes', 0) & stat.FILE_ATTRIBUTE_REPARSE_POINT):
            raise RuntimeError('Fixture cleanup refused a link/reparse point')
        if stat.S_ISDIR(info.st_mode):
            for child in sorted(path.iterdir()):
                inventory(child)
        elif not stat.S_ISREG(info.st_mode):
            raise RuntimeError('Fixture cleanup refused an unknown file type')
        entries.append((path, info))

    inventory(fixture)
    for path, before in entries:
        current = path.lstat()
        if (current.st_dev, current.st_ino, current.st_mode,
                getattr(current, 'st_file_attributes', 0)) != (
                before.st_dev, before.st_ino, before.st_mode,
                getattr(before, 'st_file_attributes', 0)):
            raise RuntimeError('Fixture changed after cleanup inventory')
        if stat.S_ISDIR(before.st_mode):
            path.rmdir()
        else:
            path.unlink()
    return len(entries)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output-dir', required=True, type=Path)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--executable', type=Path, help='使用指定的隔離建置；須搭配 --no-build')
    parser.add_argument('--benchmark', action='store_true')
    parser.add_argument('--checks', nargs='+', choices=('self-test', 'integration-test'),
                        default=['self-test', 'integration-test'], help='只復測受影響的隔離驗證階段')
    args = parser.parse_args(argv)
    if args.executable and not args.no_build:
        parser.error('--executable requires --no-build')
    output = args.output_dir.resolve()
    output.mkdir(parents=True, exist_ok=False)
    executable = args.executable.resolve() if args.executable else (
        ROOT / 'desktop/runtime/McpLocal.Desktop.exe' if args.no_build else output / 'app/McpLocal.Desktop.exe')
    modes = list(dict.fromkeys(args.checks)) + (['desktop-benchmark'] if args.benchmark else [])
    results = []
    summary = {'state': 'FAIL', 'results': results, 'executable': str(executable),
               'production_state': 'NOT_READ', 'production_connections': 'NOT_OPERATED',
               'scheduler_and_power': 'MOCKED', 'manual_acceptance': 'NOT_ASSESSED', 'actual_fps': 'NOT_MEASURED'}
    try:
        if not args.no_build:
            result = subprocess.run([
                'powershell.exe', '-NoProfile', '-File', str(ROOT / 'desktop/Build-Desktop.ps1'),
                '-OutputDirectory', str(output / 'app'), '-IntermediateDirectory', str(output / 'obj')],
                cwd=ROOT, capture_output=True, timeout=120, creationflags=subprocess.CREATE_NO_WINDOW)
            (output / 'build.txt').write_bytes(result.stdout + result.stderr)
            if result.returncode:
                raise RuntimeError('WPF build failed; see build.txt')
        for mode in modes:
            report = output / (mode + '.json')
            row = {'mode': mode, 'exit': None, 'state': 'NOT_RUN', 'cleanup': 'NOT_REQUIRED'}
            results.append(row)
            command = [str(executable), '--' + mode, '--report', str(report)]
            fixture = None
            if mode != 'self-test':
                fixture = output / (mode + '-fixture')
                fixture.mkdir(exist_ok=False)
                command += ['--fixture', str(fixture)]
                row.update(fixture=str(fixture), cleanup='RETAINED_UNTIL_EXIT_CONFIRMED')
            with subprocess.Popen(command, cwd=ROOT, creationflags=subprocess.CREATE_NO_WINDOW) as process:
                row['frontend_pid'] = process.pid
                try:
                    code = process.wait(timeout=90)
                except subprocess.TimeoutExpired:
                    # Terminate only this test frontend; pipe EOF lets its backend clean its own Job.
                    process.terminate()
                    process.wait(timeout=10)
                    raise RuntimeError('Isolated WPF check timed out; fixture retained: ' + mode)
            row['exit'] = code
            result = json.loads(report.read_text(encoding='utf-8')) if report.exists() else {}
            row['state'] = result.get('state', 'MEASURED' if mode == 'desktop-benchmark' and 'measurement' in result else 'NO_REPORT')
            if fixture is not None:
                wait_for_backend(fixture, process.pid, output)
                row['cleanup'] = 'PENDING'
            if fixture is not None:
                try:
                    row['cleanup_entries'] = cleanup_fixture(fixture, output)
                    row['cleanup'] = 'PASS'
                except (OSError, RuntimeError):
                    row['cleanup'] = 'BLOCKED'
                    raise
            expected = 'MEASURED' if mode == 'desktop-benchmark' else 'PASS'
            if code or row['state'] != expected:
                break
        if len(results) == len(modes) and all(row['exit'] == 0 and row['state'] == (
                'MEASURED' if row['mode'] == 'desktop-benchmark' else 'PASS') for row in results):
            summary['state'] = 'PASS'
    except (OSError, RuntimeError, ValueError, subprocess.SubprocessError) as error:
        summary['error'] = f'{type(error).__name__}: {error}'
    finally:
        (output / 'summary.json').write_text(json.dumps(summary, indent=2), encoding='utf-8')
    print(json.dumps(summary))
    return int(summary['state'] != 'PASS')


if __name__ == '__main__':
    raise SystemExit(main())
