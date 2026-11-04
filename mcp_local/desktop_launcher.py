"""單一 WPF 桌面入口；不載入正式設定、憑證或 Python GUI toolkit。"""
from __future__ import annotations
import argparse
import ctypes
from pathlib import Path
import subprocess
import sys

PROJECT = Path(__file__).resolve().parents[1]


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
        # The inherited stdin lifetime is owned by this launch/task process. Ending
        # it asks WPF to close, then its private backend pipe releases the owned Job.
        with subprocess.Popen(command, cwd=PROJECT, stdin=subprocess.PIPE,
                              creationflags=subprocess.CREATE_NO_WINDOW) as process:
            return process.wait()
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
