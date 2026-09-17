"""獨立還原程序；stdin 的阻塞讀取在 GUI crash/關閉 pipe 時由 OS 喚醒。"""
from pathlib import Path
import ctypes
import hashlib
import json
import sys
from power_policy import PowerSchemeManager
from power_windows import PowerError, WindowsPower


def serve(manager, source, output) -> None:
    def reply(value):
        output.write(json.dumps(value) + '\n')
        output.flush()
    try:
        manager.recover()
        reply({'ok': True})
        for line in source:
            try:
                command = json.loads(line)
                if (not isinstance(command, dict) or
                        set(command) not in ({'mode'}, {'mode', 'manage_power_scheme', 'manage_windows_power_mode'}) or
                        command.get('mode') not in ('off', 'extreme') or
                        any(type(command[k]) is not bool for k in command if k != 'mode')):
                    raise PowerError('POWER_RUNTIME_STATE_CORRUPT')
                warnings = manager.apply(command['mode'], **{k: v for k, v in command.items() if k != 'mode'})
                reply({'ok': True, 'warnings': warnings})
            except BrokenPipeError:
                raise
            except Exception as exc:
                reply({'ok': False, 'error': getattr(exc, 'code', 'POWER_SCHEME_APPLY_FAILED')})
    finally:
        manager.restore()


def main() -> int:
    from ctypes import wintypes as w
    api = WindowsPower()
    supplied = Path(sys.argv[1]).absolute()
    path = supplied.parent.resolve() / supplied.name  # Preserve final symlink for journal validation.
    create = api.kernel.CreateMutexW
    create.argtypes, create.restype = [ctypes.c_void_p, w.BOOL, w.LPCWSTR], w.HANDLE
    name = 'Local\\MCP_Power_Guard_' + hashlib.sha256(str(path).casefold().encode()).hexdigest()[:24]
    mutex = create(None, False, name)
    if not mutex or ctypes.get_last_error() == 183:
        print(json.dumps({'ok': False, 'error': 'POWER_GUARD_ALREADY_RUNNING'}), flush=True)
        if mutex:
            api.close(mutex)
        return 1
    try:
        serve(PowerSchemeManager(api, path), sys.stdin, sys.stdout)
        return 0
    except Exception as exc:
        try:
            print(json.dumps({'ok': False, 'error': getattr(exc, 'code', 'POWER_SCHEME_RESTORE_FAILED')}), flush=True)
        except OSError:
            pass
        return 1
    finally:
        api.close(mutex)


if __name__ == '__main__':
    raise SystemExit(main())
