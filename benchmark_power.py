"""明確 opt-in 的 OFF/EXTREME 實機 benchmark；會暫時改動整機電源方案。"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
from power_policy import PowerPolicyManager, MAXIMUM, EPP, BOOST, DISPLAY
from power_windows import PowerError



def power_snapshot(api) -> dict:
    """Read-only snapshot; unavailable capabilities are explicit."""
    import ctypes
    from ctypes import wintypes as w
    class Status(ctypes.Structure):
        _fields_ = [('ac', w.BYTE), ('battery', w.BYTE), ('percent', w.BYTE),
                    ('reserved', w.BYTE), ('remaining', w.DWORD), ('full', w.DWORD)]
    status = Status()
    get_status = api.kernel.GetSystemPowerStatus
    get_status.argtypes, get_status.restype = [ctypes.POINTER(Status)], w.BOOL
    supply = {0: 'DC', 1: 'AC'}.get(status.ac, 'unknown') if get_status(ctypes.byref(status)) else 'unknown'
    active = api.get_active()
    snapshot = {'windows_build': sys.getwindowsversion().build, 'supply': supply,
                'active_scheme': active, 'user_power_modes': {}, 'values': {}}
    for source in ('AC', 'DC'):
        try:
            snapshot['user_power_modes'][source] = api.get_user_power_mode(source)
        except PowerError as exc:
            snapshot['user_power_modes'][source] = exc.code
        snapshot['values'][source] = {}
        for name, setting in (('cpu_max', MAXIMUM), ('epp', EPP), ('boost', BOOST), ('display_timeout', DISPLAY)):
            try:
                value = api.read(active, *setting, source)
            except PowerError as exc:
                value = exc.code
            snapshot['values'][source][name] = value
    return snapshot

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--allow-system-power-changes', action='store_true')
    parser.add_argument('--root', required=True)
    parser.add_argument('--cases', default='benchmark-power-cases.json')
    parser.add_argument('--output-dir', required=True)
    parser.add_argument('--mode', choices=['local', 'stdio'], default='stdio')
    args = parser.parse_args()
    if not args.allow_system_power_changes or os.environ.get('MCP_LOCAL_ALLOW_POWER_INTEGRATION_TEST') != '1':
        parser.error('需明確允許系統電源變更並設定 MCP_LOCAL_ALLOW_POWER_INTEGRATION_TEST=1。')
    from connection_settings import CONFIG_DIR
    output = Path(args.output_dir).resolve()
    output.mkdir(parents=True, exist_ok=True)
    policy = PowerPolicyManager(CONFIG_DIR / 'power-runtime.json')
    policy.recover()
    reports = []
    policy.connection_started()
    try:
        token, owner = policy.connection_arguments()
        for mode in ('off', 'extreme'):
            policy.apply(mode)
            environment = os.environ.copy()
            environment.update(MCP_LOCAL_BENCHMARK_POWER=mode, MCP_LOCAL_BENCHMARK_CHANNEL=token,
                               MCP_LOCAL_BENCHMARK_OWNER=str(owner))
            result_path = output / (args.mode + '-' + mode + '.json')
            entry = {'mode': mode, 'warnings': list(policy.warnings), 'report': result_path.name,
                     'snapshot': power_snapshot(policy.backend()), 'status': 'RUNNING'}
            reports.append(entry)
            result_path.unlink(missing_ok=True)  # Do not mistake an earlier successful run for this run.
            completed = subprocess.run([sys.executable, '-B', str(Path(__file__).with_name('benchmark_e2e.py')),
                            '--mode', args.mode, '--root', args.root, '--cases', args.cases,
                            '--repeats', '100', '--output', str(result_path)], env=environment,
                           check=False, creationflags=0x08000000)
            entry['status'] = 'OK' if completed.returncode == 0 else 'FAILED'
            if completed.returncode:
                entry.update(error_count=1, disconnect_count=None)
                return 1  # Failure details are intentionally not file contents or credentials.
            entry.update(error_count=0, disconnect_count=0)
        (output / 'power-matrix.json').write_text(json.dumps(reports, indent=2), encoding='utf-8')
        return 0
    finally:
        try:
            policy.connection_stopped()
            policy.close()
        finally:
            (output / 'power-matrix.json').write_text(json.dumps(reports, indent=2), encoding='utf-8')


if __name__ == '__main__':
    raise SystemExit(main())
