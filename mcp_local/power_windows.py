"""Windows 電源 API；匯入時不修改系統狀態。"""
from __future__ import annotations
import ctypes as c
from ctypes import wintypes as w
import uuid

GUID_POWER_MODE_BEST_EFFICIENCY = '961cc777-2547-4f9d-8174-7d86181b8a7a'


class PowerError(RuntimeError):
    def __init__(self, code: str):
        self.code = code
        super().__init__(code + '：電源控制未完成，請檢查系統能力與權限。')


class Guid(c.Structure):
    _fields_ = [('data', c.c_ubyte * 16)]

    @classmethod
    def parse(cls, value: str):
        return cls.from_buffer_copy(uuid.UUID(value).bytes_le)

    def __str__(self):
        return str(uuid.UUID(bytes_le=bytes(self.data)))


class WindowsPower:
    def __init__(self):
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        self.powr = c.WinDLL('powrprof', use_last_error=True)
        def bind(lib, name, result, *args):
            fn = getattr(lib, name)
            fn.restype, fn.argtypes = result, args
            mutations = {'PowerSetActiveScheme', 'PowerDeleteScheme',
                         'PowerSetUserConfiguredACPowerMode', 'PowerSetUserConfiguredDCPowerMode'}
            if name in mutations:
                def guarded(*values):
                    import sys
                    sys.audit('mcp.power.mutate', name)
                    return fn(*values)
                return guarded
            return fn
        p = c.POINTER(Guid)
        self.active = bind(self.powr, 'PowerGetActiveScheme', w.DWORD, w.HKEY, c.POINTER(p))
        self.activate = bind(self.powr, 'PowerSetActiveScheme', w.DWORD, w.HKEY, p)
        self.delete = bind(self.powr, 'PowerDeleteScheme', w.DWORD, w.HKEY, p)
        self.free = bind(self.kernel, 'LocalFree', c.c_void_p, c.c_void_p)
        self.close = bind(self.kernel, 'CloseHandle', w.BOOL, w.HANDLE)
        self.create_event = bind(self.kernel, 'CreateEventW', w.HANDLE, c.c_void_p, w.BOOL, w.BOOL, w.LPCWSTR)
        self.set_event = bind(self.kernel, 'SetEvent', w.BOOL, w.HANDLE)
        self.reset_event = bind(self.kernel, 'ResetEvent', w.BOOL, w.HANDLE)
        self.wait = bind(self.kernel, 'WaitForMultipleObjects', w.DWORD, w.DWORD,
                         c.POINTER(w.HANDLE), w.BOOL, w.DWORD)
        self.wait_one = bind(self.kernel, 'WaitForSingleObject', w.DWORD, w.HANDLE, w.DWORD)
        self.open_process = bind(self.kernel, 'OpenProcess', w.HANDLE, w.DWORD, w.BOOL, w.DWORD)
        self.times = bind(self.kernel, 'GetProcessTimes', w.BOOL, w.HANDLE,
                          *([c.POINTER(w.FILETIME)] * 4))
        self.user_power_mode = {}
        self.user_power_mode_set = {}
        for supply in ('AC', 'DC'):
            get_name = 'PowerGetUserConfigured' + supply + 'PowerMode'
            set_name = 'PowerSetUserConfigured' + supply + 'PowerMode'
            get_fn = getattr(self.powr, get_name, None)
            set_fn = getattr(self.powr, set_name, None)
            if get_fn and set_fn:
                get_fn.restype, get_fn.argtypes = w.DWORD, [c.POINTER(Guid)]
                set_fn.restype, set_fn.argtypes = w.DWORD, [c.POINTER(Guid)]
                self.user_power_mode[supply] = get_fn
                self.user_power_mode_set[supply] = bind(self.powr, set_name, w.DWORD, c.POINTER(Guid))

    def supports_user_power_mode(self) -> bool:
        return set(self.user_power_mode) == set(self.user_power_mode_set) == {'AC', 'DC'}

    def get_user_power_mode(self, supply: str) -> str:
        fn = self.user_power_mode.get(supply)
        if fn is None:
            raise PowerError('POWER_USER_MODE_UNAVAILABLE')
        value = Guid()
        if fn(c.byref(value)):
            raise PowerError('POWER_USER_MODE_READ_FAILED')
        return str(value)

    def set_user_power_mode(self, supply: str, value: str) -> None:
        fn = self.user_power_mode_set.get(supply)
        if fn is None:
            raise PowerError('POWER_USER_MODE_UNAVAILABLE')
        if fn(c.byref(Guid.parse(value))):
            raise PowerError('POWER_USER_MODE_APPLY_FAILED')

    def get_active(self) -> str:
        result = c.POINTER(Guid)()
        if self.active(None, c.byref(result)):
            raise PowerError('POWER_SCHEME_READ_FAILED')
        try:
            return str(result.contents)
        finally:
            self.free(result)

    def set_active(self, scheme: str) -> None:
        if self.activate(None, c.byref(Guid.parse(scheme))):
            raise PowerError('POWER_SCHEME_APPLY_FAILED')

    def delete_scheme(self, scheme: str) -> None:
        # Already removed by a previous recovery is safe.
        if self.delete(None, c.byref(Guid.parse(scheme))) not in (0, 2):
            raise PowerError('POWER_SCHEME_DELETE_FAILED')

    def process_identity(self, handle) -> int:
        values = [w.FILETIME() for _ in range(4)]
        if not self.times(handle, *[c.byref(v) for v in values]):
            raise PowerError('POWER_RUNTIME_STATE_CORRUPT')
        return (values[0].dwHighDateTime << 32) | values[0].dwLowDateTime


class CancelEvent:
    """threading.Event-compatible cancel with a native wait handle, lazily allocated."""
    def __init__(self):
        import threading
        self.event = threading.Event()
        self.lock = threading.Lock()
        self.handle = None
        self.api = None

    def set(self) -> None:
        with self.lock:
            self.event.set()
            if self.handle and not self.api.set_event(self.handle):
                raise PowerError('POWER_EVENT_FAILED')

    def clear(self) -> None:
        with self.lock:
            self.event.clear()
            if self.handle and not self.api.reset_event(self.handle):
                raise PowerError('POWER_EVENT_FAILED')

    def is_set(self) -> bool:
        return self.event.is_set()

    def wait(self, timeout=None) -> bool:
        return self.event.wait(timeout)

    def wait_process(self, process, timeout=None) -> bool:
        with self.lock:
            if self.handle is None:
                self.api = WindowsPower()
                self.handle = self.api.create_event(None, True, self.is_set(), None)
                if not self.handle:
                    raise PowerError('POWER_EVENT_FAILED')
        handles = (w.HANDLE * 2)(self.handle, int(process._handle))
        result = self.api.wait(2, handles, False, 0xffffffff if timeout is None else max(0, round(timeout * 1000)))
        if result == 258:
            return False
        if result not in (0, 1):
            raise PowerError('POWER_EVENT_FAILED')
        if result == 1:
            process.wait()
        return True

    def close(self) -> None:
        with self.lock:
            if self.handle:
                self.api.close(self.handle)
                self.handle = None


