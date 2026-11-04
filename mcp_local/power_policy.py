"""僅回復舊版電源日誌；不套用新的電源模式。"""
from __future__ import annotations
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
from enum import Enum
from mcp_local.power_windows import PowerError, GUID_POWER_MODE_BEST_EFFICIENCY


class PowerMode(str, Enum):
    OFF = 'off'
    EXTREME = 'extreme'


class PowerSchemeManager:
    """Validate and recover a legacy journal without creating new transactions."""
    def __init__(self, api, path: Path):
        self.api, self.path = api, path
        self.state = None

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

    def apply(self, mode: str, **_options) -> list[str]:
        if mode != 'off':
            raise ValueError('電源模式已移除。')
        self.restore()
        return []

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
        self.options = None
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
        if mode != 'off':
            raise ValueError('電源模式已移除。')
        self.start()
        try:
            self.process.stdin.write(json.dumps({'mode': mode}) + '\n')
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


