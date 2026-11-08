"""單一 WPF 桌面入口；不載入正式設定、憑證或 Python GUI toolkit。"""
from __future__ import annotations
import argparse
import ctypes
import os
import shutil
from pathlib import Path
import subprocess
import sys

PROJECT = Path(__file__).resolve().parents[1]


def check_desktop_runtime() -> None:
    """Check the x64 host and both frameworks without starting/loading the desktop."""
    host = Path(os.environ.get('ProgramW6432', os.environ.get('ProgramFiles', r'C:\Program Files'))) / 'dotnet/dotnet.exe'
    executable = str(host) if host.is_file() else shutil.which('dotnet')
    message = ('需要 .NET 10 LTS Windows Desktop Runtime（x64）。請安裝 Desktop Runtime，'
               '一般 .NET Runtime 不包含 WPF；從來源建置另需 .NET 10 SDK。')
    if not executable:
        raise OSError(message)
    try:
        result = subprocess.run([executable, '--list-runtimes'], capture_output=True, text=True,
                                timeout=20, creationflags=subprocess.CREATE_NO_WINDOW)
        frameworks = {line.split()[0] for line in result.stdout.splitlines()
                      if len(line.split()) > 1 and line.split()[1].startswith('10.0.') and '-' not in line.split()[1]}
        if result.returncode or not {'Microsoft.NETCore.App', 'Microsoft.WindowsDesktop.App'} <= frameworks:
            raise OSError(message)
    except subprocess.SubprocessError as error:
        raise OSError(message + ' 執行環境檢查未完成。') from error


def main(*, startup: bool = False, argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--fixture', type=Path)
    parser.add_argument('--fixture-load', action='store_true')
    parser.add_argument('--fixture-native-tray', action='store_true')
    parser.add_argument('--integration-test', action='store_true')
    parser.add_argument('--self-test', action='store_true')
    parser.add_argument('--desktop-benchmark', action='store_true')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args(argv)
    if (args.fixture_load or args.fixture_native_tray or args.integration_test or args.desktop_benchmark) and not args.fixture:
        parser.error('此驗證模式必須指定新建空白 --fixture 目錄。')
    executable = PROJECT / 'desktop/runtime/McpLocal.Desktop.exe'
    command = [str(executable), '--watch-owner-stdin']
    if startup:
        command.append('--startup')
    for name in ('fixture', 'report'):
        if value := getattr(args, name):
            command.extend(['--' + name, str(value.resolve())])
    for name in ('fixture_load', 'fixture_native_tray', 'integration_test', 'self_test', 'desktop_benchmark'):
        if getattr(args, name):
            command.append('--' + name.replace('_', '-'))
    try:
        if not executable.is_file():
            raise FileNotFoundError('找不到 WPF 桌面程式；請執行 desktop\\Build-Desktop.ps1 或使用完整部署包。')
        check_desktop_runtime()
        # The inherited stdin lifetime is owned by this launch/task process. Ending
        # it asks WPF to close, then its private backend pipe releases the owned Job.
        with subprocess.Popen(command, cwd=PROJECT, stdin=subprocess.PIPE,
                              creationflags=subprocess.CREATE_NO_WINDOW) as process:
            code = process.wait()
            if code & 0xFFFFFFFF in (0x80008096, 0x8000809C):
                raise OSError('WPF 執行環境無法載入；請安裝或修復 .NET 10 Windows Desktop Runtime（x64）。')
            return code
    except OSError as error:
        message = '無法啟動 WPF 桌面：' + str(error)
        if args.fixture or args.self_test:
            if sys.stderr:
                print(message, file=sys.stderr)
        else:
            ctypes.windll.user32.MessageBoxW(None, message, 'MCP-Local', 0x10)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
