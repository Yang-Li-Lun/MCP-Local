"""桌面控制器的值、對話及單執行緒排程介面，不依賴 GUI toolkit。"""
from __future__ import annotations

import heapq
import itertools
import time
import threading
from typing import Any, Callable


class DesktopWake:
    """Wake the owning Windows message loop for worker I/O and native tray input."""
    def __init__(self):
        import ctypes as c
        from ctypes import wintypes as w
        self.c, self.w = c, w
        self.event = threading.Event()
        self.lock = threading.Lock()
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        self.user = c.WinDLL('user32', use_last_error=True)
        for name, result, arguments in (
            ('CreateEventW', w.HANDLE, (c.c_void_p, w.BOOL, w.BOOL, w.LPCWSTR)),
            ('SetEvent', w.BOOL, (w.HANDLE,)), ('ResetEvent', w.BOOL, (w.HANDLE,)),
            ('CloseHandle', w.BOOL, (w.HANDLE,))):
            function = getattr(self.kernel, name)
            function.restype, function.argtypes = result, arguments
        self.user.MsgWaitForMultipleObjectsEx.restype = w.DWORD
        self.user.MsgWaitForMultipleObjectsEx.argtypes = (w.DWORD, c.POINTER(w.HANDLE), w.DWORD, w.DWORD, w.DWORD)
        self.handle = self.kernel.CreateEventW(None, True, False, None)
        if not self.handle:
            raise c.WinError(c.get_last_error())

    def set(self):
        with self.lock:
            self.event.set()
            if self.handle:
                self.kernel.SetEvent(self.handle)

    def clear(self):
        with self.lock:
            self.event.clear()
            if self.handle:
                self.kernel.ResetEvent(self.handle)

    def is_set(self):
        return self.event.is_set()

    def wait(self, timeout=1):
        handles = (self.w.HANDLE * 1)(self.handle)
        state = self.user.MsgWaitForMultipleObjectsEx(1, handles, max(0, round(timeout * 1000)), 0x04ff, 4)
        if state == 0xffffffff:
            raise self.c.WinError(self.c.get_last_error())
        return state != 258

    def close(self):
        with self.lock:
            if self.handle:
                self.kernel.CloseHandle(self.handle)
                self.handle = None


class Value:
    def __init__(self, value: Any = '', changed: Callable | None = None):
        self.value, self.changed = value, changed

    def get(self, *args) -> Any:
        return self.value

    def set(self, value: Any) -> None:
        if value != self.value:
            self.value = value
            if self.changed:
                self.changed()


class TextValue(Value):
    def delete(self, *args) -> None:
        self.set('')

    def insert(self, index: str, text: str) -> None:
        self.set(self.value + text if index == 'end' else text + self.value)

    def edit_modified(self, value: bool = False) -> bool:
        return False


class Scheduler:
    def __init__(self, emit: Callable[[dict], None] | None = None):
        self.emit = emit or (lambda event: None)
        self.timers: list = []
        self.cancelled: set = set()
        self.identifiers = itertools.count()
        self.visible = True
        self.closed = False

    def after(self, milliseconds: int, callback: Callable) -> int:
        identifier = next(self.identifiers)
        heapq.heappush(self.timers, (time.monotonic() + milliseconds / 1000, identifier, callback))
        return identifier

    def after_cancel(self, identifier: int) -> None:
        self.cancelled.add(identifier)

    def update(self) -> None:
        # Snapshot deadline prevents callbacks scheduled by callbacks from starving I/O.
        now = time.monotonic()
        for _ in range(100):
            if not self.timers or self.timers[0][0] > now or self.closed:
                return
            _, identifier, callback = heapq.heappop(self.timers)
            if identifier in self.cancelled:
                self.cancelled.remove(identifier)
            else:
                callback()

    update_idletasks = update

    def state(self) -> str:
        return 'normal' if self.visible else 'withdrawn'

    def withdraw(self) -> None:
        self.visible = False
        self.emit({'event': 'visibility', 'visible': False})

    def deiconify(self) -> None:
        self.visible = True
        self.emit({'event': 'visibility', 'visible': True})

    def lift(self) -> None:
        pass

    def destroy(self) -> None:
        self.closed = True
        self.timers.clear()
        self.cancelled.clear()


class Dialogs:
    """預設拒絕確認；正式執行由私有管線提供 WPF 對話實作。"""
    def askyesno(self, title: str, text: str, **kwargs) -> bool:
        return False

    def showerror(self, title: str, text: str, **kwargs) -> None:
        pass

    showinfo = showerror
    showwarning = showerror
