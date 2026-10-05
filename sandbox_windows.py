"""Windows AppContainer：無網路 capability、專用 profile、顯式 handle 清單。

依 Microsoft Win32 API 建立；失敗時絕不改以主機權限執行。
只對新建沙箱工作目錄設定 ACL，不變更共享根、使用者或系統 ACL。
"""
from __future__ import annotations

import base64
import ctypes as c
from ctypes import wintypes as w
import os
from pathlib import Path
import subprocess
import uuid

V = c.c_void_p
P = c.POINTER


class SecurityAttributes(c.Structure):
    _fields_ = [('length', w.DWORD), ('descriptor', V), ('inherit', w.BOOL)]


class StartupInfo(c.Structure):
    _fields_ = [('cb', w.DWORD), ('reserved', w.LPWSTR), ('desktop', w.LPWSTR),
                ('title', w.LPWSTR), ('x', w.DWORD), ('y', w.DWORD),
                ('xs', w.DWORD), ('ys', w.DWORD), ('xc', w.DWORD), ('yc', w.DWORD),
                ('fill', w.DWORD), ('flags', w.DWORD), ('show', w.WORD),
                ('reserved2', w.WORD), ('reserved3', V), ('stdin', w.HANDLE),
                ('stdout', w.HANDLE), ('stderr', w.HANDLE)]


class StartupInfoEx(c.Structure):
    _fields_ = [('startup', StartupInfo), ('attributes', V)]


class ProcessInfo(c.Structure):
    _fields_ = [('process', w.HANDLE), ('thread', w.HANDLE), ('pid', w.DWORD), ('tid', w.DWORD)]


class SecurityCapabilities(c.Structure):
    _fields_ = [('sid', V), ('capabilities', V), ('count', w.DWORD), ('reserved', w.DWORD)]


def check(value) -> None:
    if not value:
        raise c.WinError(c.get_last_error())


class WindowsApi:
    def __init__(self):
        if os.name != 'nt':
            raise ValueError('SANDBOX_UNAVAILABLE：需要 Windows AppContainer。')
        self.kernel = c.WinDLL('kernel32', use_last_error=True)
        self.userenv = c.WinDLL('userenv', use_last_error=True)
        self.advapi = c.WinDLL('advapi32', use_last_error=True)
        self.ole = c.WinDLL('ole32', use_last_error=True)
        def bind(lib, name, result, *arguments):
            function = getattr(lib, name)
            function.restype, function.argtypes = result, arguments
        k, u, a = self.kernel, self.userenv, self.advapi
        bind(u, 'CreateAppContainerProfile', w.LONG, w.LPCWSTR, w.LPCWSTR, w.LPCWSTR, V, w.DWORD, P(V))
        bind(u, 'DeleteAppContainerProfile', w.LONG, w.LPCWSTR)
        bind(u, 'GetAppContainerFolderPath', w.LONG, w.LPCWSTR, P(V))
        bind(a, 'ConvertSidToStringSidW', w.BOOL, V, P(V))
        bind(a, 'FreeSid', V, V)
        bind(a, 'EqualSid', w.BOOL, V, V)
        bind(a, 'OpenProcessToken', w.BOOL, w.HANDLE, w.DWORD, P(w.HANDLE))
        bind(a, 'GetTokenInformation', w.BOOL, w.HANDLE, c.c_int, V, w.DWORD, P(w.DWORD))
        bind(k, 'LocalFree', V, V)
        bind(self.ole, 'CoTaskMemFree', None, V)
        bind(k, 'CloseHandle', w.BOOL, w.HANDLE)
        bind(k, 'CreatePipe', w.BOOL, P(w.HANDLE), P(w.HANDLE), V, w.DWORD)
        bind(k, 'SetHandleInformation', w.BOOL, w.HANDLE, w.DWORD, w.DWORD)
        bind(k, 'InitializeProcThreadAttributeList', w.BOOL, V, w.DWORD, w.DWORD, P(c.c_size_t))
        bind(k, 'UpdateProcThreadAttribute', w.BOOL, V, w.DWORD, c.c_size_t, V, c.c_size_t, V, V)
        bind(k, 'DeleteProcThreadAttributeList', None, V)
        bind(k, 'CreateProcessW', w.BOOL, w.LPCWSTR, w.LPWSTR, V, V, w.BOOL, w.DWORD,
             V, w.LPCWSTR, P(StartupInfoEx), P(ProcessInfo))
        bind(k, 'ResumeThread', w.DWORD, w.HANDLE)
        bind(k, 'WaitForSingleObject', w.DWORD, w.HANDLE, w.DWORD)
        bind(k, 'TerminateProcess', w.BOOL, w.HANDLE, w.UINT)
        bind(k, 'GetExitCodeProcess', w.BOOL, w.HANDLE, P(w.DWORD))
        bind(k, 'SetInformationJobObject', w.BOOL, w.HANDLE, c.c_int, V, w.DWORD)
        bind(k, 'GetWindowsDirectoryW', w.UINT, w.LPWSTR, w.UINT)
        buffer = c.create_unicode_buffer(32768)
        count = k.GetWindowsDirectoryW(buffer, len(buffer))
        if not 0 < count < len(buffer):
            raise ValueError('SANDBOX_UNAVAILABLE：無法定位 Windows。')
        self.windows = Path(buffer.value)


class Sandbox:
    def __init__(self):
        self.api = WindowsApi()
        if c.windll.shell32.IsUserAnAdmin():
            raise ValueError('SANDBOX_ELEVATED：請以一般使用者執行完整控制模式。')
        self.name = 'MCP-Local-Session-' + uuid.uuid4().hex
        self.sid = V()
        self._handle = None
        self.job = None
        self.stdin = self.stdout = None
        self.pid = None
        self.profile_created = False
        result = self.api.userenv.CreateAppContainerProfile(
            self.name, self.name, 'MCP-Local isolated terminal', None, 0, c.byref(self.sid))
        if result < 0:
            raise ValueError(f'SANDBOX_UNAVAILABLE：建立 AppContainer 失敗（{result & 0xffffffff:08x}）。')
        self.profile_created = True
        sid_string, folder = V(), V()
        try:
            check(self.api.advapi.ConvertSidToStringSidW(self.sid, c.byref(sid_string)))
            result = self.api.userenv.GetAppContainerFolderPath(c.wstring_at(sid_string), c.byref(folder))
            if result < 0:
                raise ValueError('SANDBOX_UNAVAILABLE：無法取得沙箱儲存區。')
            self.profile = Path(c.wstring_at(folder))
            self.workspace = self.profile / 'workspace'
            self.workspace.mkdir()
            # This is a newly created private directory, never a caller-supplied path.
            grant = subprocess.run([
                str(self.api.windows / 'System32/icacls.exe'), str(self.workspace),
                '/grant', '*' + c.wstring_at(sid_string) + ':(OI)(CI)M',
                '/setintegritylevel', '(OI)(CI)L'], capture_output=True,
                creationflags=subprocess.CREATE_NO_WINDOW, timeout=10, check=False)
            if grant.returncode:
                raise ValueError('SANDBOX_UNAVAILABLE：無法授權沙箱工作目錄。')
        except BaseException:
            self.close()
            raise
        finally:
            if folder:
                self.api.ole.CoTaskMemFree(folder)
            if sid_string:
                self.api.kernel.LocalFree(sid_string)

    def verify_token(self) -> None:
        token = w.HANDLE()
        check(self.api.advapi.OpenProcessToken(self._handle, 8, c.byref(token)))
        try:
            flag, length = w.DWORD(), w.DWORD()
            check(self.api.advapi.GetTokenInformation(token, 29, c.byref(flag), c.sizeof(flag), c.byref(length)))
            if flag.value != 1:
                raise ValueError('SANDBOX_TOKEN：程序不是 AppContainer。')
            self.api.advapi.GetTokenInformation(token, 31, None, 0, c.byref(length))
            buffer = c.create_string_buffer(length.value)
            check(self.api.advapi.GetTokenInformation(token, 31, buffer, len(buffer), c.byref(length)))
            actual = c.cast(buffer, P(V))[0]
            if not self.api.advapi.EqualSid(actual, self.sid):
                raise ValueError('SANDBOX_TOKEN：沙箱身分不符。')
        finally:
            self.api.kernel.CloseHandle(token)

    def start(self, command: str) -> None:
        import msvcrt
        from tray_windows import Job, ExtendedLimit
        if self._handle:
            raise ValueError('沙箱已啟動。')
        kernel = self.api.kernel
        handles = []
        attrs = None
        thread = None
        try:
            sa = SecurityAttributes(c.sizeof(SecurityAttributes), None, True)
            out_read, out_write, in_read, in_write = (w.HANDLE() for _ in range(4))
            check(kernel.CreatePipe(c.byref(out_read), c.byref(out_write), c.byref(sa), 0))
            handles.extend([out_read, out_write])
            check(kernel.CreatePipe(c.byref(in_read), c.byref(in_write), c.byref(sa), 0))
            handles.extend([in_read, in_write])
            check(kernel.SetHandleInformation(out_read, 1, 0))
            check(kernel.SetHandleInformation(in_write, 1, 0))
            size = c.c_size_t()
            kernel.InitializeProcThreadAttributeList(None, 2, 0, c.byref(size))
            buffer = c.create_string_buffer(size.value)
            check(kernel.InitializeProcThreadAttributeList(buffer, 2, 0, c.byref(size)))
            attrs = buffer
            caps = SecurityCapabilities(self.sid, None, 0, 0)
            check(kernel.UpdateProcThreadAttribute(attrs, 0, 0x20009, c.byref(caps), c.sizeof(caps), None, None))
            inherited = (w.HANDLE * 2)(out_write.value, in_read.value)
            check(kernel.UpdateProcThreadAttribute(attrs, 0, 0x20002, inherited, c.sizeof(inherited), None, None))
            startup = StartupInfoEx()
            startup.startup.cb = c.sizeof(startup)
            startup.attributes = c.cast(attrs, V)
            startup.startup.flags = 0x100
            startup.startup.stdin, startup.startup.stdout, startup.startup.stderr = in_read, out_write, out_write
            executable = str(self.api.windows / 'System32/WindowsPowerShell/v1.0/powershell.exe')
            prelude = (
                "$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Stop'; "
                "[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); "
                "[Console]::InputEncoding=[Text.UTF8Encoding]::new(); "
                "$OutputEncoding=[Console]::OutputEncoding; "
                "$PSDefaultParameterValues=@{'Get-Content:Encoding'='utf8'; 'Set-Content:Encoding'='utf8'; "
                "'Add-Content:Encoding'='utf8'; 'Out-File:Encoding'='utf8'}; "
                "try { New-PSDrive -Name work -PSProvider FileSystem -Root $env:MCP_WORKSPACE | Out-Null; "
                "Set-Location work:\\; [IO.Directory]::SetCurrentDirectory($env:MCP_WORKSPACE); & {\n")
            script = prelude + command + "\n} } catch { [Console]::WriteLine($_.ToString()); exit 1 }"
            args = [executable, '-NoLogo', '-NoProfile', '-NonInteractive', '-InputFormat', 'Text',
                    '-OutputFormat', 'Text', '-EncodedCommand', base64.b64encode(script.encode('utf-16-le')).decode('ascii')]
            command_line = subprocess.list2cmdline(args)
            if len(command_line.encode('utf-16-le')) // 2 > 30000:
                raise ValueError('COMMAND_LIMIT：命令過長。')
            env = dict(SystemRoot=str(self.api.windows), WINDIR=str(self.api.windows),
                       PATH=str(self.api.windows / 'System32'),
                       PSModulePath=str(self.api.windows / 'System32/WindowsPowerShell/v1.0/Modules'),
                       TEMP=str(self.workspace), TMP=str(self.workspace),
                       USERPROFILE=str(self.workspace), LOCALAPPDATA=str(self.workspace),
                       APPDATA=str(self.workspace), MCP_WORKSPACE=str(self.workspace),
                       COMSPEC=str(self.api.windows / 'System32/cmd.exe'))
            environment = c.create_unicode_buffer('\0'.join(k + '=' + v for k, v in sorted(env.items())) + '\0\0')
            process = ProcessInfo()
            # Suspended until the Job and actual token have been verified.
            check(kernel.CreateProcessW(executable, c.create_unicode_buffer(command_line), None, None, True,
                                        0x80000 | 0x400 | 0x08000000 | 4, environment, str(self.workspace),
                                        c.byref(startup), c.byref(process)))
            self._handle, self.pid, thread = process.process, process.pid, process.thread
            self.job = Job()
            limits = ExtendedLimit()
            limits.basic.flags = 0x2000 | 0x8 | 0x200  # kill-on-close, active processes, total memory
            limits.basic.active = 8
            limits.job_memory = 512 * 1024 * 1024
            check(kernel.SetInformationJobObject(self.job.handle, 9, c.byref(limits), c.sizeof(limits)))
            self.job.assign(self)
            self.verify_token()
            self.stdout = os.fdopen(msvcrt.open_osfhandle(out_read.value, os.O_RDONLY | os.O_BINARY), 'rb', buffering=0)
            handles.remove(out_read)
            self.stdin = os.fdopen(msvcrt.open_osfhandle(in_write.value, os.O_WRONLY | os.O_BINARY), 'wb', buffering=0)
            handles.remove(in_write)
            if kernel.ResumeThread(thread) == 0xffffffff:
                raise c.WinError(c.get_last_error())
        except BaseException:
            self.close()
            raise
        finally:
            if thread:
                kernel.CloseHandle(thread)
            if attrs is not None:
                kernel.DeleteProcThreadAttributeList(attrs)
            for handle in handles:
                kernel.CloseHandle(handle)

    def poll(self) -> int | None:
        if not self._handle or self.api.kernel.WaitForSingleObject(self._handle, 0) != 0:
            return None
        code = w.DWORD()
        check(self.api.kernel.GetExitCodeProcess(self._handle, c.byref(code)))
        return code.value

    def stop(self) -> None:
        if self.job:
            self.job.stop()
        elif self._handle:
            check(self.api.kernel.TerminateProcess(self._handle, 1))
        if self._handle and self.api.kernel.WaitForSingleObject(self._handle, 5000) != 0:
            raise ValueError('SESSION_STOP_FAILED：沙箱程序尚未停止。')

    def close(self) -> None:
        if self._handle:
            self.stop()
        if self.job:
            self.job.close()
            self.job = None
        for stream in (self.stdin, self.stdout):
            if stream:
                stream.close()
        self.stdin = self.stdout = None
        if self._handle:
            self.api.kernel.CloseHandle(self._handle)
            self._handle = None
        if self.profile_created:
            result = self.api.userenv.DeleteAppContainerProfile(self.name)
            if result < 0:
                raise ValueError('SANDBOX_CLEANUP_FAILED：沙箱 profile 未移除，請保留診斷資訊。')
            self.profile_created = False
        if self.sid:
            self.api.advapi.FreeSid(self.sid)
            self.sid = V()
