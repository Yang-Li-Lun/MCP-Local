"""預設完整隔離回歸；禁止測試開啟正式狀態檔或執行正式通道與排程命令。"""
import ast
import importlib.metadata
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import unittest


def main():
    root = Path(__file__).resolve().parent
    os.chdir(root)
    state = (Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local').resolve()
    def guard(event, arguments):
        if event == 'open' and isinstance(arguments[0], (str, bytes, os.PathLike)):
            path = Path(os.fsdecode(arguments[0])).absolute()
            if path.is_relative_to(state):
                raise RuntimeError('ISOLATION_GUARD：禁止測試讀寫正式狀態檔。')
        if event == 'subprocess.Popen':
            executable = Path(str(arguments[0])).name.casefold()
            if executable in ('schtasks.exe', 'tunnel-client.exe', 'cloudflared.exe'):
                raise RuntimeError('ISOLATION_GUARD：禁止正式排程或通道命令。')
    sys.addaudithook(guard)
    sources = sorted(list(root.glob('*.py')) + list((root / 'tests').glob('*.py')))
    for path in sources:
        ast.parse(path.read_text(encoding='utf-8-sig'), filename=path.name)
    dependencies = subprocess.run([sys.executable, '-B', '-m', 'pip', 'check'], capture_output=True, text=True)
    started = time.monotonic()
    with (root / 'engineering-tests.txt').open('w', encoding='utf-8') as log:
        result = unittest.TextTestRunner(stream=log, verbosity=2).run(unittest.defaultTestLoader.discover('tests'))
    report = {'python': sys.version.split()[0], 'mcp': importlib.metadata.version('mcp'),
              'pydantic': importlib.metadata.version('pydantic'), 'syntax_files': len(sources),
              'pip_check': dependencies.returncode, 'tests': result.testsRun,
              'failures': len(result.failures), 'errors': len(result.errors),
              'skipped': len(result.skipped), 'elapsed_seconds': round(time.monotonic()-started, 3),
              'remote_acceptance': 'NOT_RUN', 'production_scheduler': 'NOT_RUN',
              'isolation': 'audit guard for main test process; subprocess fixtures separately reviewed'}
    (root / 'engineering-results.json').write_text(json.dumps(report, indent=2), encoding='utf-8')
    print(json.dumps(report))
    return 0 if result.wasSuccessful() and dependencies.returncode == 0 else 1


if __name__ == '__main__':
    raise SystemExit(main())
