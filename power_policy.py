"""可回復電源策略；預設 OFF，原生操作採延遲初始化。"""
from __future__ import annotations
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
from enum import Enum
from power_windows import PowerError, WindowsPower, ModeChannel, GUID_POWER_MODE_BEST_EFFICIENCY


class PowerMode(str, Enum):
    OFF = 'off'
    EXTREME = 'extreme'


POWER_DEFAULTS = {'mode': 'off', 'restore_original_plan': True, 'keep_system_awake': True,
                  'manage_power_scheme': True, 'manage_windows_power_mode': True,
                  'advanced_job_cpu_cap_percent': None}


def normalize_power(value: dict | None = None) -> dict:
    if value is None:
        value = {}
    if not isinstance(value, dict) or set(value) - set(POWER_DEFAULTS):
        raise ValueError('電源設定格式錯誤。')
    result = {**POWER_DEFAULTS, **value}
    if result['mode'] not in ('off', 'extreme'):
        raise ValueError('電源模式必須為 off 或 extreme。')
    for field in ('restore_original_plan', 'keep_system_awake', 'manage_power_scheme', 'manage_windows_power_mode'):
        if type(result[field]) is not bool:
            raise ValueError('電源選項必須為布林值。')
    # These are safety invariants, not supported opt-outs.
    if not result['restore_original_plan'] or not result['keep_system_awake']:
        raise ValueError('不可停用電源方案還原或連線保持喚醒。')
    cap = result['advanced_job_cpu_cap_percent']
    if cap is not None and (type(cap) is not int or not 10 <= cap <= 100):
        raise ValueError('CPU 配額必須為 10 至 100 的整數或 null。')
    return result


SLEEP = ('238c9fa8-0aad-41ed-83f4-97be242c8f20', '29f6c1db-86da-48c5-9fdb-f2b67b1f44da')
DISPLAY = ('7516b95f-f776-4464-8c53-06167f40cc99', '3c0bc021-c8a8-4e07-a973-6b14cbcb2b7e')
CPU = '54533251-82be-4824-96c1-47b60b740d00'
EPP = (CPU, '36687f9e-e3a5-4dbf-b1dc-15eb381c6863')
MAXIMUM = (CPU, 'bc5038f7-23e0-4960-96da-33abaf5935ec')
BOOST = (CPU, 'be337238-0d82-4146-a960-4f3749d470c7')


class PowerSchemeManager:
    """Guard-owned scheme transaction; journal survives guard/GUI crashes."""
    def __init__(self, api, path: Path):
        self.api, self.path = api, path
        self.state = None

    def journal(self) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        import tempfile
        if self.path.is_symlink():
            raise PowerError('POWER_RUNTIME_STATE_CORRUPT')
        descriptor, name = tempfile.mkstemp(prefix=self.path.name + '.', suffix='.tmp',
                                            dir=self.path.parent)
        temporary = Path(name)
        try:
            with os.fdopen(descriptor, 'w', encoding='utf-8') as stream:
                json.dump(self.state, stream)
                stream.flush()
                os.fsync(stream.fileno())
            temporary.replace(self.path)
        finally:
            temporary.unlink(missing_ok=True)

    def recover(self) -> None:
        if not self.path.exists() and not self.path.is_symlink():
            return
        import uuid
        try:
            if self.path.is_symlink() or self.path.stat().st_size > 4096:
                raise ValueError()
            def unique_keys(pairs):
                result = {}
                for key, value in pairs:
                    if key in result:
                        raise ValueError()
                    result[key] = value
                return result
            with self.path.open('rb') as stream:
                data = stream.read(4097)
            if len(data) > 4096:
                raise ValueError()
            state = json.loads(data.decode('utf-8'), object_pairs_hook=unique_keys)
            base = {'owner_pid', 'owner_created', 'original', 'owned', 'mode'}
            extra = {'journal_version', 'user_power_mode_supported', 'original_ac_power_mode',
                     'original_dc_power_mode', 'applied_power_mode'}
            if not isinstance(state, dict) or set(state) not in (base, base | extra):
                raise ValueError()
            version = state.get('journal_version', 1)
            if type(version) is not int or version not in (1, 2):
                raise ValueError()
            if version == 2:
                if set(state) != base | extra or type(state['user_power_mode_supported']) is not bool:
                    raise ValueError()
                for field in ('original_ac_power_mode', 'original_dc_power_mode'):
                    if state['user_power_mode_supported']:
                        state[field] = str(uuid.UUID(state[field]))
                    elif state[field] is not None:
                        raise ValueError()
                if state['applied_power_mode'] != GUID_POWER_MODE_BEST_EFFICIENCY:
                    raise ValueError()
            elif set(state) != base:
                raise ValueError()
            if type(state['owner_pid']) is not int or type(state['owner_created']) is not int:
                raise ValueError()
            if not 0 < state['owner_pid'] <= 0xffffffff or not 0 <= state['owner_created'] <= 0xffffffffffffffff:
                raise ValueError()
            state['original'] = str(uuid.UUID(state['original']))
            if not isinstance(state['owned'], list) or not (1 if version == 1 else 0) <= len(state['owned']) <= 8:
                raise ValueError()
            state['owned'] = [str(uuid.UUID(value)) for value in state['owned']]
            if state['original'] in state['owned'] or len(set(state['owned'])) != len(state['owned']):
                raise ValueError()
            if version == 1 and state['mode'] == 'low':
                state['mode'] = 'off'  # Legacy journal is recovery-only.
            PowerMode(state['mode'])
        except (ValueError, TypeError, KeyError, OSError, AttributeError, UnicodeError):
            raise PowerError('POWER_RUNTIME_STATE_CORRUPT') from None
        handle = self.api.open_process(0x100000 | 0x1000, False, state['owner_pid'])
        if handle:
            try:
                if self.api.process_identity(handle) == state['owner_created'] and self.api.wait_one(handle, 0) == 258:
                    raise PowerError('POWER_GUARD_ALREADY_RUNNING')
            finally:
                self.api.close(handle)
        else:
            import ctypes
            if ctypes.get_last_error() == 5:
                raise PowerError('POWER_GUARD_ALREADY_RUNNING')
        self.state = state
        self.restore()

    def configure(self, scheme: str, original: str, mode: str) -> list[str]:
        if mode != 'extreme':
            raise ValueError('只有極致節能可配置暫時方案。')
        warnings = []
        for index, supply in enumerate(('AC', 'DC')):
            self.api.write(scheme, *SLEEP, supply, 0)
            value = self.api.read(original, *DISPLAY, supply)
            maximum = (300, 120)[index]
            self.api.write(scheme, *DISPLAY, supply, min(value, maximum) if value else maximum)
            for setting, values in ((EPP, (90, 100)),
                                    (MAXIMUM, (80, 60)),
                                    (BOOST, (0, 0))):
                try:
                    self.api.read(original, *setting, supply)
                except PowerError as exc:
                    if exc.code != 'POWER_CAPABILITY_UNAVAILABLE':
                        raise
                    warnings.append('POWER_CAPABILITY_UNAVAILABLE:' + setting[1] + ':' + supply)
                    continue
                self.api.write(scheme, *setting, supply, values[index])
        return warnings

    def apply(self, mode: str, *, manage_power_scheme: bool = True,
              manage_windows_power_mode: bool = True) -> list[str]:
        PowerMode(mode)
        if mode == 'off':
            self.restore()
            return []
        # Finish an earlier transaction before taking a new snapshot.
        if self.state:
            self.restore()
        elif self.path.exists() or self.path.is_symlink():
            self.recover()
        warnings = []
        original = self.api.get_active()
        supported = False
        originals = {'AC': None, 'DC': None}
        if manage_windows_power_mode:
            if not self.api.supports_user_power_mode():
                warnings.append('POWER_USER_MODE_UNAVAILABLE')
            else:
                try:
                    originals = {supply: self.api.get_user_power_mode(supply)
                                 for supply in ('AC', 'DC')}
                    supported = True
                except PowerError:
                    warnings.append('POWER_USER_MODE_READ_FAILED')
                    originals = {'AC': None, 'DC': None}
        identity = self.api.process_identity(self.api.current())
        candidate = self.api.copy_scheme(original) if manage_power_scheme else None
        self.state = {
            'journal_version': 2, 'owner_pid': os.getpid(), 'owner_created': identity,
            'original': original, 'owned': [candidate] if candidate else [], 'mode': mode,
            'user_power_mode_supported': supported,
            'original_ac_power_mode': originals['AC'], 'original_dc_power_mode': originals['DC'],
            'applied_power_mode': GUID_POWER_MODE_BEST_EFFICIENCY,
        }
        try:
            self.journal()  # Durable intent precedes activation and both user-mode writes.
            if candidate:
                warnings.extend(self.configure(candidate, original, mode))
                self.api.set_active(candidate)
        except Exception as error:
            try:
                self.restore()
            except Exception:
                raise PowerError('POWER_ROLLBACK_FAILED') from error
            raise
        if supported:
            for supply in ('AC', 'DC'):
                try:
                    self.api.set_user_power_mode(supply, GUID_POWER_MODE_BEST_EFFICIENCY)
                except PowerError:
                    warnings.append('POWER_USER_MODE_APPLY_FAILED:' + supply)
        return warnings

    def restore(self) -> None:
        if not self.state:
            return
        errors = []
        if self.state.get('user_power_mode_supported'):
            for supply in ('AC', 'DC'):
                try:
                    if self.api.get_user_power_mode(supply) == self.state['applied_power_mode']:
                        self.api.set_user_power_mode(
                            supply, self.state['original_' + supply.lower() + '_power_mode'])
                except Exception:
                    errors.append('POWER_USER_MODE_RESTORE_FAILED')
        # Respect a power scheme explicitly selected by the user while the app ran.
        try:
            if self.api.get_active() in self.state['owned']:
                self.api.set_active(self.state['original'])
        except Exception:
            errors.append('POWER_SCHEME_RESTORE_FAILED')
        if not errors:
            for scheme in self.state['owned']:
                try:
                    self.api.delete_scheme(scheme)
                except Exception:
                    errors.append('POWER_SCHEME_DELETE_FAILED')
        if errors:
            raise PowerError(errors[0])  # Keep the complete journal for retry/recovery.
        self.path.unlink(missing_ok=True)
        self.state = None


class PowerRestoreGuardClient:
    """Anonymous pipe EOF is the crash signal; no timer or shared secret is needed."""
    def __init__(self, path: Path, options=None):
        self.options = normalize_power(options)
        self.path = path
        self.process = None
        self.responses = queue.Queue()

    def start(self) -> None:
        if self.process:
            if self.process.poll() is not None:
                raise PowerError('POWER_GUARD_START_FAILED')
            return
        # Explicit environment allowlist: the guard never receives tunnel credentials.
        environment = {key: value for key, value in os.environ.items()
                       if key.upper() in {'SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP', 'LOCALAPPDATA'}}
        self.process = subprocess.Popen([sys.executable, '-B', str(Path(__file__).with_name('power_restore_guard.py')),
                                         str(self.path)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, text=True, encoding='utf-8',
                                        env=environment, close_fds=True, creationflags=0x08000000)
        def read():
            try:
                for line in self.process.stdout:
                    self.responses.put(line)
            finally:
                self.responses.put(None)
        threading.Thread(target=read, daemon=True).start()
        self.response()

    def response(self):
        try:
            line = self.responses.get(timeout=15)
            value = json.loads(line) if line else {}
        except (queue.Empty, ValueError):
            self.abort()
            raise PowerError('POWER_GUARD_START_FAILED') from None
        if not value.get('ok'):
            raise PowerError(value.get('error', 'POWER_GUARD_START_FAILED'))
        return value.get('warnings', [])

    def apply(self, mode: str) -> list[str]:
        self.start()
        try:
            self.process.stdin.write(json.dumps({'mode': mode, 'manage_power_scheme': self.options['manage_power_scheme'],
                                                 'manage_windows_power_mode': self.options['manage_windows_power_mode']}) + '\n')
            self.process.stdin.flush()
        except (OSError, ValueError):
            raise PowerError('POWER_GUARD_START_FAILED') from None
        return self.response()

    def abort(self) -> None:
        if self.process and self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()  # guard restores on EOF, including response timeout

    def close(self) -> None:
        if self.process:
            self.apply('off')
            self.abort()
            try:
                self.process.wait(timeout=15)
            except subprocess.TimeoutExpired:
                raise PowerError('POWER_SCHEME_RESTORE_FAILED') from None
            self.process.stdout.close()
            self.process = None


class PowerPolicyManager:
    def __init__(self, path: Path, *, api=None, guard=None, options=None):
        self.path = path
        self.api = api
        self.guard = guard or PowerRestoreGuardClient(path, options)
        self.options = normalize_power(options)
        self.mode = PowerMode.OFF
        self.connections = 0
        self.request_handle = None
        self.qos_owned = False
        self.channel = None
        self.lock = threading.RLock()
        self.warnings = []
        self.visible = True

    def backend(self):
        if self.api is None:
            self.api = WindowsPower()
        return self.api

    def recover(self) -> None:
        if self.path.exists() or self.path.is_symlink():
            self.guard.start()  # guard mutex serializes stale recovery with live guard

    def apply(self, mode: PowerMode | str) -> None:
        mode = PowerMode(mode)
        with self.lock:
            if mode == self.mode:
                return
            old = self.mode
            warnings = []
            if self.options['advanced_job_cpu_cap_percent'] is not None:
                warnings.append('POWER_CPU_CAP_NOT_APPLIED')
            acquired = False
            try:
                if mode != PowerMode.OFF and self.connections and not self.request_handle:
                    self.request_handle = self.backend().request()
                    acquired = True
                if self.options['manage_power_scheme'] or self.options['manage_windows_power_mode']:
                    warnings.extend(self.guard.apply(mode.value))
                if self.channel:
                    self.channel.publish(mode.value)
                if mode != PowerMode.OFF or self.qos_owned:
                    try:
                        self.backend().qos(mode != PowerMode.OFF)
                        self.qos_owned = mode != PowerMode.OFF
                    except PowerError as exc:
                        if mode == PowerMode.OFF:
                            raise
                        warnings.append(exc.code)
                if mode == PowerMode.OFF and self.request_handle:
                    self.backend().release_request(self.request_handle)
                    self.request_handle = None
            except Exception as error:
                try:
                    if self.options['manage_power_scheme'] or self.options['manage_windows_power_mode']:
                        self.guard.apply(old.value)
                    if self.channel:
                        self.channel.publish(old.value)
                    self.backend().qos(old != PowerMode.OFF)
                    self.qos_owned = old != PowerMode.OFF
                    if acquired:
                        self.backend().release_request(self.request_handle)
                        self.request_handle = None
                except Exception:
                    raise PowerError('POWER_ROLLBACK_FAILED') from error
                raise
            self.mode, self.warnings = mode, warnings

    def connection_started(self) -> None:
        with self.lock:
            if self.mode != PowerMode.OFF and not self.request_handle:
                self.request_handle = self.backend().request()
            self.connections += 1

    def connection_stopped(self) -> None:
        with self.lock:
            self.connections = max(0, self.connections - 1)
            if not self.connections and self.request_handle:
                self.backend().release_request(self.request_handle)
                self.request_handle = None

    def connection_arguments(self) -> tuple[str, int]:
        with self.lock:
            if self.channel is None:
                self.channel = ModeChannel(self.backend(), self.mode.value)
            return self.channel.token, os.getpid()

    def set_window_visible(self, visible: bool) -> None:
        self.visible = visible

    def close(self) -> None:
        with self.lock:
            if self.connections:
                raise PowerError('POWER_CONNECTION_STILL_RUNNING')
            self.apply(PowerMode.OFF)
            if self.request_handle:
                self.backend().release_request(self.request_handle)
                self.request_handle = None
            self.guard.close()
            if self.channel:
                self.channel.close()
                self.channel = None


def tick_interval(mode: str, visible: bool, state: str, busy: bool, pending: bool) -> int:
    PowerMode(mode)
    if pending:
        return 50
    if busy or state in ('STARTING', 'STOPPING', 'SAVING', 'RESTARTING', 'QUITTING'):
        return 100
    index = ('off', 'extreme').index(mode)
    if visible:
        return (250, 1000)[index]
    return (500, 5000)[index] if state == 'RUNNING' else (1000, 5000)[index]
