"""受信任的主機命令：目前使用者 token、有界 Job、明確 pipe handles。

這不是檔案或憑證沙箱。命令具備目前使用者的檔案權限。
不使用 ShellExecute/runas，不提升權限；子程序環境僅繼承明列欄位。
"""
from __future__ import annotations

import base64
import ctypes as c
from ctypes import wintypes as w
import os
from pathlib import Path
import subprocess

from sandbox_windows import WindowsApi, SecurityAttributes, StartupInfoEx, ProcessInfo, V, check

ENVIRONMENT_NAMES = frozenset({
    'SYSTEMROOT', 'WINDIR', 'SYSTEMDRIVE', 'COMSPEC', 'PATH', 'PATHEXT',
    'PROGRAMFILES', 'PROGRAMFILES(X86)', 'PROGRAMW6432', 'PROGRAMDATA',
    'COMMONPROGRAMFILES', 'COMMONPROGRAMFILES(X86)', 'COMMONPROGRAMW6432',
    'USERPROFILE', 'HOMEDRIVE', 'HOMEPATH', 'APPDATA', 'LOCALAPPDATA', 'TEMP', 'TMP',
    'PROCESSOR_ARCHITECTURE', 'PROCESSOR_IDENTIFIER', 'PROCESSOR_LEVEL',
    'PROCESSOR_REVISION', 'NUMBER_OF_PROCESSORS', 'OS', 'PSMODULEPATH',
})


def host_environment(workspace: Path, windows: Path) -> dict[str, str]:
    """不繼承任意金鑰變數；使用者 profile 內的憑證仍可被受信任命令存取。"""
    environment = {key.upper(): value for key, value in os.environ.items()
                   if key.upper() in ENVIRONMENT_NAMES}
    environment.update(SYSTEMROOT=str(windows), WINDIR=str(windows),
                       COMSPEC=str(windows / 'System32/cmd.exe'),
                       MCP_WORKSPACE=str(workspace), PYTHONIOENCODING='utf-8', PYTHONUTF8='1')
    return environment


class HostProcess:
    def __init__(self, workspace: Path):
        self.api = WindowsApi()
        self.workspace = workspace
        self._handle = self.job = self.stdin = self.stdout = None
        self.pid = None
        self.verify_current_token(w.HANDLE(-1))

    def verify_current_token(self, process: w.HANDLE) -> None:
        """啟動前及 resume 前核對未提升、非 AppContainer、相同 Windows 使用者。"""
        tokens = []
        try:
            for handle in (w.HANDLE(-1), process):
                token = w.HANDLE()
                check(self.api.advapi.OpenProcessToken(handle, 8, c.byref(token)))
                tokens.append(token)
                for field, expected in ((20, 0), (29, 0)):  # elevation, AppContainer
                    flag, length = w.DWORD(), w.DWORD()
                    check(self.api.advapi.GetTokenInformation(
                        token, field, c.byref(flag), c.sizeof(flag), c.byref(length)))
                    if flag.value != expected:
                        raise ValueError('HOST_TOKEN_REJECTED：主機模式需要目前未提升的一般使用者。')
            buffers = []
            for token in tokens:
                length = w.DWORD()
                self.api.advapi.GetTokenInformation(token, 1, None, 0, c.byref(length))
                buffer = c.create_string_buffer(length.value)
                check(self.api.advapi.GetTokenInformation(token, 1, buffer, len(buffer), c.byref(length)))
                buffers.append(buffer)
            if not self.api.advapi.EqualSid(c.cast(buffers[0], c.POINTER(V))[0],
                                          c.cast(buffers[1], c.POINTER(V))[0]):
                raise ValueError('HOST_TOKEN_REJECTED：子程序使用者不符。')
        finally:
            for token in tokens:
                self.api.kernel.CloseHandle(token)

    def start(self, command: str) -> None:
        import msvcrt
        from tray_windows import Job, ExtendedLimit
        if self._handle:
            raise ValueError('SESSION_STARTED：程序已啟動。')
        kernel, handles, attrs, thread = self.api.kernel, [], None, None
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
            kernel.InitializeProcThreadAttributeList(None, 1, 0, c.byref(size))
            buffer = c.create_string_buffer(size.value)
            check(kernel.InitializeProcThreadAttributeList(buffer, 1, 0, c.byref(size)))
            attrs = buffer
            inherited = (w.HANDLE * 2)(out_write.value, in_read.value)
            check(kernel.UpdateProcThreadAttribute(attrs, 0, 0x20002, inherited, c.sizeof(inherited), None, None))
            startup = StartupInfoEx()
            startup.startup.cb = c.sizeof(startup)
            startup.attributes = c.cast(attrs, V)
            startup.startup.flags = 0x100
            startup.startup.stdin, startup.startup.stdout, startup.startup.stderr = in_read, out_write, out_write
            executable = str(self.api.windows / 'System32/WindowsPowerShell/v1.0/powershell.exe')
            script = ("$ProgressPreference='SilentlyContinue'; $ErrorActionPreference='Stop'; "
                      "[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); "
                      "[Console]::InputEncoding=[Text.UTF8Encoding]::new(); "
                      "$OutputEncoding=[Console]::OutputEncoding; "
                      "$PSDefaultParameterValues=@{'Get-Content:Encoding'='utf8'; 'Set-Content:Encoding'='utf8'; "
                      "'Add-Content:Encoding'='utf8'; 'Out-File:Encoding'='utf8'}; "
                      "try { Set-Location -LiteralPath $env:MCP_WORKSPACE; "
                      "[IO.Directory]::SetCurrentDirectory($env:MCP_WORKSPACE); & {\n" + command +
                      "\n} } catch { [Console]::WriteLine($_.ToString()); exit 1 }")
            args = [executable, '-NoLogo', '-NoProfile', '-NonInteractive', '-InputFormat', 'Text',
                    '-OutputFormat', 'Text', '-EncodedCommand', base64.b64encode(script.encode('utf-16-le')).decode('ascii')]
            command_line = subprocess.list2cmdline(args)
            if len(command_line.encode('utf-16-le')) // 2 > 30000:
                raise ValueError('COMMAND_LIMIT：命令過長。')
            env = host_environment(self.workspace, self.api.windows)
            environment = c.create_unicode_buffer('\0'.join(k + '=' + v for k, v in sorted(env.items())) + '\0\0')
            process = ProcessInfo()
            check(kernel.CreateProcessW(executable, c.create_unicode_buffer(command_line), None, None, True,
                                        0x80000 | 0x400 | 0x08000000 | 4, environment, str(self.workspace),
                                        c.byref(startup), c.byref(process)))
            self._handle, self.pid, thread = process.process, process.pid, process.thread
            self.job = Job()
            limits = ExtendedLimit()
            limits.basic.flags = 0x2000 | 0x8 | 0x200
            limits.basic.active = 32
            limits.job_memory = 4 * 1024 * 1024 * 1024
            check(kernel.SetInformationJobObject(self.job.handle, 9, c.byref(limits), c.sizeof(limits)))
            self.job.assign(self)
            self.verify_current_token(self._handle)
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
            raise ValueError('SESSION_STOP_FAILED：主機程序尚未停止。')

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
