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


class Throttling(c.Structure):
    _fields_ = [('version', w.DWORD), ('control', w.DWORD), ('state', w.DWORD)]


class DetailedReason(c.Structure):
    _fields_ = [('module', w.HMODULE), ('reason_id', w.ULONG), ('count', w.ULONG),
                ('strings', c.POINTER(w.LPWSTR))]


class ReasonValue(c.Union):
    _fields_ = [('text', w.LPWSTR), ('detailed', DetailedReason)]


class Reason(c.Structure):
    _anonymous_ = ('value',)
    _fields_ = [('version', w.ULONG), ('flags', w.DWORD), ('value', ReasonValue)]


class WindowsPower:
    def __init__(self):
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        self.powr = c.WinDLL('powrprof', use_last_error=True)
        def bind(lib, name, result, *args):
            fn = getattr(lib, name)
            fn.restype, fn.argtypes = result, args
            mutations = {'PowerDuplicateScheme', 'PowerSetActiveScheme', 'PowerDeleteScheme',
                         'PowerWriteACValueIndex', 'PowerWriteDCValueIndex', 'SetProcessInformation',
                         'PowerCreateRequest', 'PowerSetRequest', 'PowerClearRequest',
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
        self.duplicate = bind(self.powr, 'PowerDuplicateScheme', w.DWORD, w.HKEY, p, c.POINTER(p))
        self.activate = bind(self.powr, 'PowerSetActiveScheme', w.DWORD, w.HKEY, p)
        self.delete = bind(self.powr, 'PowerDeleteScheme', w.DWORD, w.HKEY, p)
        self.reads, self.writes = {}, {}
        for supply in ('AC', 'DC'):
            self.reads[supply] = bind(self.powr, 'PowerRead' + supply + 'ValueIndex', w.DWORD,
                                      w.HKEY, p, p, p, c.POINTER(w.DWORD))
            self.writes[supply] = bind(self.powr, 'PowerWrite' + supply + 'ValueIndex', w.DWORD,
                                       w.HKEY, p, p, p, w.DWORD)
        self.free = bind(self.kernel, 'LocalFree', c.c_void_p, c.c_void_p)
        self.current = bind(self.kernel, 'GetCurrentProcess', w.HANDLE)
        self.info = bind(self.kernel, 'SetProcessInformation', w.BOOL, w.HANDLE, c.c_int, c.c_void_p, w.DWORD)
        self.create_request = bind(self.kernel, 'PowerCreateRequest', w.HANDLE, c.POINTER(Reason))
        self.set_request = bind(self.kernel, 'PowerSetRequest', w.BOOL, w.HANDLE, c.c_int)
        self.clear_request = bind(self.kernel, 'PowerClearRequest', w.BOOL, w.HANDLE, c.c_int)
        self.close = bind(self.kernel, 'CloseHandle', w.BOOL, w.HANDLE)
        self.create_event = bind(self.kernel, 'CreateEventW', w.HANDLE, c.c_void_p, w.BOOL, w.BOOL, w.LPCWSTR)
        self.open_event = bind(self.kernel, 'OpenEventW', w.HANDLE, w.DWORD, w.BOOL, w.LPCWSTR)
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

    def copy_scheme(self, source: str) -> str:
        result = c.POINTER(Guid)()
        if self.duplicate(None, c.byref(Guid.parse(source)), c.byref(result)):
            raise PowerError('POWER_SCHEME_DUPLICATE_FAILED')
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

    def read(self, scheme: str, group: str, setting: str, supply: str) -> int:
        result = w.DWORD()
        if self.reads[supply](None, *[c.byref(Guid.parse(v)) for v in (scheme, group, setting)], c.byref(result)):
            raise PowerError('POWER_CAPABILITY_UNAVAILABLE')
        return result.value

    def write(self, scheme: str, group: str, setting: str, supply: str, value: int) -> None:
        if self.writes[supply](None, *[c.byref(Guid.parse(v)) for v in (scheme, group, setting)], value):
            raise PowerError('POWER_SCHEME_APPLY_FAILED')

    def qos(self, enabled: bool) -> None:
        state = Throttling(1, 1 if enabled else 0, 1 if enabled else 0)
        if not self.info(self.current(), 4, c.byref(state), c.sizeof(state)):
            raise PowerError('POWER_QOS_APPLY_FAILED' if enabled else 'POWER_QOS_RESTORE_FAILED')

    def request(self):
        reason = Reason(version=0, flags=1, text='MCP-Local 連線保持系統喚醒')
        handle = self.create_request(c.byref(reason))
        if not handle or handle == c.c_void_p(-1).value:
            raise PowerError('POWER_REQUEST_CREATE_FAILED')
        if not self.set_request(handle, 1):  # SystemRequired only; never DisplayRequired.
            self.close(handle)
            raise PowerError('POWER_REQUEST_SET_FAILED')
        return handle

    def release_request(self, handle) -> None:
        if not self.clear_request(handle, 1):
            raise PowerError('POWER_REQUEST_CLEAR_FAILED')
        self.close(handle)

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


class ModeChannel:
    """Two manual-reset events broadcast a mode to every MCP child without polling."""
    modes = ('off', 'extreme')

    def __init__(self, api, mode: str):
        if mode not in self.modes:
            raise ValueError('無效的電源模式。')
        self.api = api
        self.token = uuid.uuid4().hex
        self.handles = []
        try:
            for item in self.modes:
                handle = api.create_event(None, True, item == mode, self.name(self.token, item))
                if not handle:
                    raise PowerError('POWER_EVENT_FAILED')
                self.handles.append(handle)
        except Exception:
            self.close()
            raise

    @staticmethod
    def name(token: str, mode: str) -> str:
        return 'Local\\MCP_Local_Power_' + token + '_' + mode

    def publish(self, mode: str) -> None:
        if mode not in self.modes:
            raise ValueError('無效的電源模式。')
        for item, handle in zip(self.modes, self.handles):
            if item != mode and not self.api.reset_event(handle):
                raise PowerError('POWER_EVENT_FAILED')
        if not self.api.set_event(self.handles[self.modes.index(mode)]):
            raise PowerError('POWER_EVENT_FAILED')

    def close(self) -> None:
        for handle in self.handles:
            self.api.close(handle)
        self.handles.clear()


def follow_mode(token: str, owner: int) -> None:
    """MCP-side daemon: only changes its own QoS, never reads settings or keys."""
    import threading
    if not token or len(token) != 32 or any(ch not in '0123456789abcdef' for ch in token):
        raise ValueError('無效的電源模式通道。')
    api = WindowsPower()
    parent = api.open_process(0x100000, False, owner)
    if not parent:
        raise PowerError('POWER_EVENT_FAILED')
    handles = []
    try:
        for mode in ModeChannel.modes:
            handle = api.open_event(0x100000, False, ModeChannel.name(token, mode))
            if not handle:
                raise PowerError('POWER_EVENT_FAILED')
            handles.append(handle)
    except Exception:
        for handle in [parent, *handles]:
            api.close(handle)
        raise
    initial = (w.HANDLE * 3)(parent, *handles)
    selected = api.wait(len(initial), initial, False, 0xffffffff)
    if selected not in (1, 2):
        for handle in [parent, *handles]:
            api.close(handle)
        raise PowerError('POWER_EVENT_FAILED')
    try:
        api.qos(selected == 2)
    except PowerError as exc:
        import sys
        print(exc.code, file=sys.stderr)
    def run():
        current = selected - 1
        try:
            while True:
                indexes = [i for i in range(2) if i != current]
                waits = (w.HANDLE * (len(indexes) + 1))(parent, *[handles[i] for i in indexes])
                result = api.wait(len(waits), waits, False, 0xffffffff)
                if result == 0:
                    break
                if result > len(indexes):
                    raise PowerError('POWER_EVENT_FAILED')
                current = indexes[result - 1]
                try:
                    api.qos(current == 1)
                except PowerError as exc:
                    import sys
                    print(exc.code, file=sys.stderr)
        finally:
            try:
                api.qos(False)
            finally:
                for handle in [parent, *handles]:
                    api.close(handle)
    threading.Thread(target=run, name='mcp-power-mode', daemon=True).start()
