"""Windows 系統匣與子程序生命週期；不需要額外套件。"""
import ctypes as c
from ctypes import wintypes as w
from collections import deque

user = c.WinDLL('user32', use_last_error=True)
kernel = c.WinDLL('kernel32', use_last_error=True)
shell = c.WinDLL('shell32', use_last_error=True)
LRESULT = c.c_ssize_t
WNDPROC = c.WINFUNCTYPE(LRESULT, w.HWND, w.UINT, w.WPARAM, w.LPARAM)


class WindowClass(c.Structure):
    _fields_ = [('style', w.UINT), ('proc', WNDPROC), ('extra', c.c_int),
                ('window_extra', c.c_int), ('instance', w.HINSTANCE),
                ('icon', w.HICON), ('cursor', w.HANDLE), ('background', w.HBRUSH),
                ('menu', w.LPCWSTR), ('name', w.LPCWSTR)]


class NotifyIcon(c.Structure):
    _fields_ = [('size', w.DWORD), ('window', w.HWND), ('id', w.UINT),
                ('flags', w.UINT), ('message', w.UINT), ('icon', w.HICON),
                ('tip', w.WCHAR * 128), ('state', w.DWORD), ('mask', w.DWORD),
                ('info', w.WCHAR * 256), ('version', w.UINT),
                ('title', w.WCHAR * 64), ('info_flags', w.DWORD),
                ('guid', c.c_byte * 16), ('balloon', w.HICON)]


class BasicLimit(c.Structure):
    _fields_ = [('process_time', c.c_int64), ('job_time', c.c_int64),
                ('flags', w.DWORD), ('min_ws', c.c_size_t), ('max_ws', c.c_size_t),
                ('active', w.DWORD), ('affinity', c.c_size_t),
                ('priority', w.DWORD), ('scheduling', w.DWORD)]


class ExtendedLimit(c.Structure):
    _fields_ = [('basic', BasicLimit), ('io', c.c_uint64 * 6),
                ('process_memory', c.c_size_t), ('job_memory', c.c_size_t),
                ('peak_process', c.c_size_t), ('peak_job', c.c_size_t)]


def signature(lib, name, result, *args):
    """明確設定 64 位元 Windows API 的參數與回傳型別。"""
    function = getattr(lib, name)
    function.restype = result
    function.argtypes = args
    return function


signature(kernel, 'CreateJobObjectW', w.HANDLE, c.c_void_p, w.LPCWSTR)
signature(kernel, 'SetInformationJobObject', w.BOOL, w.HANDLE, c.c_int,
          c.c_void_p, w.DWORD)
signature(kernel, 'AssignProcessToJobObject', w.BOOL, w.HANDLE, w.HANDLE)
signature(kernel, 'TerminateJobObject', w.BOOL, w.HANDLE, w.UINT)
signature(kernel, 'CloseHandle', w.BOOL, w.HANDLE)
signature(kernel, 'CreateMutexW', w.HANDLE, c.c_void_p, w.BOOL, w.LPCWSTR)
signature(kernel, 'GetModuleHandleW', w.HMODULE, w.LPCWSTR)
signature(user, 'RegisterClassW', w.ATOM, c.POINTER(WindowClass))
signature(user, 'CreateWindowExW', w.HWND, w.DWORD, w.LPCWSTR, w.LPCWSTR,
          w.DWORD, c.c_int, c.c_int, c.c_int, c.c_int,
          w.HWND, w.HMENU, w.HINSTANCE, c.c_void_p)
signature(user, 'DefWindowProcW', LRESULT, w.HWND, w.UINT, w.WPARAM, w.LPARAM)
signature(user, 'LoadIconW', w.HICON, w.HINSTANCE, c.c_void_p)
signature(user, 'DestroyWindow', w.BOOL, w.HWND)
signature(user, 'RegisterWindowMessageW', w.UINT, w.LPCWSTR)
signature(user, 'PeekMessageW', w.BOOL, c.POINTER(w.MSG), w.HWND, w.UINT, w.UINT, w.UINT)
signature(user, 'TranslateMessage', w.BOOL, c.POINTER(w.MSG))
signature(user, 'DispatchMessageW', LRESULT, c.POINTER(w.MSG))
signature(user, 'CreatePopupMenu', w.HMENU)
signature(user, 'AppendMenuW', w.BOOL, w.HMENU, w.UINT, c.c_size_t, w.LPCWSTR)
signature(user, 'DestroyMenu', w.BOOL, w.HMENU)
signature(user, 'GetCursorPos', w.BOOL, c.POINTER(w.POINT))
signature(user, 'SetForegroundWindow', w.BOOL, w.HWND)
signature(user, 'TrackPopupMenuEx', w.UINT, w.HMENU, w.UINT, c.c_int,
          c.c_int, w.HWND, c.c_void_p)
signature(user, 'PostMessageW', w.BOOL, w.HWND, w.UINT, w.WPARAM, w.LPARAM)
signature(shell, 'Shell_NotifyIconW', w.BOOL, w.DWORD, c.POINTER(NotifyIcon))


class Job:
    """工作物件關閉時終止本次啟動的所有子程序。"""
    def __init__(self):
        self.handle = kernel.CreateJobObjectW(None, None)
        if not self.handle:
            raise c.WinError(c.get_last_error())
        limits = ExtendedLimit()
        limits.basic.flags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        if not kernel.SetInformationJobObject(self.handle, 9, c.byref(limits), c.sizeof(limits)):
            self.close()
            raise OSError('無法設定子程序清理機制。')

    def assign(self, process) -> None:
        if not kernel.AssignProcessToJobObject(self.handle, w.HANDLE(int(process._handle))):
            raise c.WinError(c.get_last_error())

    def stop(self) -> None:
        if self.handle and not kernel.TerminateJobObject(self.handle, 1):
            raise c.WinError(c.get_last_error())

    def close(self) -> None:
        if self.handle:
            kernel.CloseHandle(self.handle)
            self.handle = None


class Tray:
    """左鍵開啟設定，右鍵開啟操作選單；Explorer 重啟時恢復圖示。"""
    def __init__(self, show, menu):
        self.show = show
        self.menu = menu
        self.events = deque()
        self.restart = user.RegisterWindowMessageW('TaskbarCreated')
        self.callback = WNDPROC(self._message)
        instance = kernel.GetModuleHandleW(None)
        self.class_name = 'MCP_Local_Tray_Window'
        window_class = WindowClass(proc=self.callback, instance=instance, name=self.class_name)
        if not user.RegisterClassW(c.byref(window_class)):
            raise c.WinError(c.get_last_error())
        self.window = user.CreateWindowExW(0, self.class_name, '', 0, 0, 0, 0, 0,
                                           None, None, instance, None)
        if not self.window:
            raise c.WinError(c.get_last_error())
        self.data = NotifyIcon(size=c.sizeof(NotifyIcon), window=self.window, id=1,
                               flags=7, message=0x8001,
                               icon=user.LoadIconW(None, c.c_void_p(32516)),
                               tip='MCP-Local：尚未啟動')
        if not shell.Shell_NotifyIconW(0, c.byref(self.data)):
            user.DestroyWindow(self.window)
            raise OSError('無法建立系統匣圖示，請重新啟動程式。')

    def _message(self, hwnd, message, wp, lp):
        if message == self.restart and hasattr(self, 'data'):
            shell.Shell_NotifyIconW(0, c.byref(self.data))
        elif message == 0x8001:
            if lp in (0x202, 0x203):
                self.events.append('show')
            elif lp == 0x205:
                self.events.append('menu')
            return 0
        return user.DefWindowProcW(hwnd, message, wp, lp)

    def update(self, text: str) -> None:
        self.data.tip = ('MCP-Local：' + text)[:127]
        shell.Shell_NotifyIconW(1, c.byref(self.data))

    def popup(self, actions: list) -> None:
        """使用隱藏的系統匣視窗承載原生選單，不顯示設定視窗。"""
        menu = user.CreatePopupMenu()
        if not menu:
            raise c.WinError(c.get_last_error())
        selected = 0
        try:
            for index, (label, enabled, _) in enumerate(actions, 1):
                flags = 0x800 if not label else (0 if enabled else 0x1)
                if not user.AppendMenuW(menu, flags, index, label):
                    raise c.WinError(c.get_last_error())
            point = w.POINT()
            if not user.GetCursorPos(c.byref(point)):
                raise c.WinError(c.get_last_error())
            user.SetForegroundWindow(self.window)
            # TPM_RETURNCMD | TPM_RIGHTBUTTON；使用原生選單迴圈處理點選。
            selected = user.TrackPopupMenuEx(menu, 0x102, point.x, point.y, self.window, None)
            user.PostMessageW(self.window, 0, 0, 0)
        finally:
            user.DestroyMenu(menu)
        if 1 <= selected <= len(actions):
            _, enabled, callback = actions[selected - 1]
            if enabled and callback:
                callback()

    def pump(self) -> None:
        message = w.MSG()
        while user.PeekMessageW(c.byref(message), self.window, 0, 0, 1):
            user.TranslateMessage(c.byref(message))
            user.DispatchMessageW(c.byref(message))
        # Windows 通知可能由 Tcl 原生訊息迴圈分派，WNDPROC 內不得再進入 Tk。
        # 在 Tk 的定時回呼中取出事件，避免 Python/Tcl 的 GIL 狀態衝突。
        while self.events:
            event = self.events.popleft()
            if event == 'show':
                self.show()
            elif event == 'menu':
                self.menu()

    def close(self) -> None:
        shell.Shell_NotifyIconW(2, c.byref(self.data))
        user.DestroyWindow(self.window)
