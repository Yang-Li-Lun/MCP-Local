"""Windows Sandbox guest I/O；主機只解析有界資料，從不執行 guest 輸出。"""
from __future__ import annotations

import base64
from contextlib import ExitStack
import json
import os
from pathlib import Path, PureWindowsPath
import queue
import shutil
import stat
import threading
import time
import uuid
import xml.etree.ElementTree as ET

from control_files import locked_path
from developer_vm_client import GuardOwner, GuardClient, GUARD_ENV
from local_files_mcp import FileReader, linked

MAX_OUTPUT_BYTES = 1024 * 1024


def remove_owned_tree(path: Path) -> None:
    """僅供已確認停止、且呼叫端已核對身分的自有暫存樹；不追蹤連結。"""
    root = path.resolve(strict=True)
    if root != path.absolute() or root == Path(root.anchor) or linked(path):
        raise ValueError('VM_SPOOL_CHANGED')
    def onerror(function, name, error_info):
        target = Path(name)
        info = target.lstat()
        if (not isinstance(error_info[1], PermissionError) or not stat.S_ISREG(info.st_mode)
                or info.st_nlink != 1 or linked(target, info)
                or not getattr(info, 'st_file_attributes', 0) & 1
                or not target.resolve(strict=True).is_relative_to(root)):
            raise error_info[1]
        # Git and similar tools mark their private object files read-only. Never
        # clear attributes on a hardlink, which could alter an external object.
        target.chmod(stat.S_IREAD | stat.S_IWRITE)
        function(name)
    shutil.rmtree(path, onerror=onerror)


class OutputPipe:
    def __init__(self):
        self.queue = queue.Queue()
        self.closed = False

    def read(self, length: int) -> bytes:
        value = self.queue.get()
        return value if value is not None else b''

    def close(self) -> None:
        self.closed = True


class InputPipe:
    def __init__(self, process):
        self.process = process
        self.index = 0
        self.closed = False
        self.lock = threading.Lock()

    def send(self, message: dict) -> None:
        self.process.publish(f'stdin-{self.index:06d}.json', message)
        self.index += 1

    def write(self, data: bytes) -> int:
        with self.lock:
            if self.closed or self.process.finished.is_set():
                raise ValueError('VM_STDIN_CLOSED')
            self.send({'data': base64.b64encode(data).decode('ascii')})
            return len(data)

    def close(self) -> None:
        with self.lock:
            if not self.closed:
                self.closed = True
                if not self.process.finished.is_set():
                    self.send({'eof': True})


class VMProcess:
    def __init__(self, files, root_id: str | None, working_directory: str | None,
                 toolchains: list[Path], lifetime: int):
        self.files, self.root_id, self.toolchains = files, root_id, toolchains
        selected = files.reader(root_id)
        cwd = selected.checked(working_directory or '.')
        if not cwd.is_dir():
            raise ValueError('VM_CWD：工作路徑必須為共享根內既有目錄。')
        self.workspace = cwd
        self.roots = [reader.root for reader in files.workspace.readers.values()]
        self.root_index = self.roots.index(selected.root)
        self.guest_roots = {identifier: f'C:\\MCP\\mapping-{index}'
                            for index, identifier in enumerate(files.workspace.readers)}
        self.guest_toolchains = [f'C:\\MCP\\mapping-{len(self.roots) + index}'
                                for index in range(len(toolchains))]
        self.guest_cwd = str(PureWindowsPath(f'C:/MCP/mapping-{self.root_index}') / cwd.relative_to(selected.root))
        self.spool_name = 'mcp-vm-session-' + uuid.uuid4().hex
        self.spool = selected.root / self.spool_name
        self.guest_spool = f'C:\\MCP\\mapping-{self.root_index}\\{self.spool_name}'
        self.stdin, self.stdout = InputPipe(self), OutputPipe()
        self.pid = None
        self.vm_id = None
        self.cancelled, self.finished = threading.Event(), threading.Event()
        self.code = None
        self.error = None
        self.lifetime = lifetime
        self.offset = 0
        self.thread = None
        self.spool_reader = None

    def publish(self, name: str, value: dict) -> None:
        raw = json.dumps(value, ensure_ascii=False)
        temporary = self.spool_name + '/pending-' + uuid.uuid4().hex + '.json'
        created = self.files.write(temporary, raw, 'create', None, self.root_id)
        self.files.move(temporary, self.spool_name + '/' + name, created['sha256'], self.root_id)

    def configuration(self) -> str:
        from developer_control import build_vm_mapping_plan
        config = ET.fromstring(build_vm_mapping_plan(self.roots, self.toolchains))
        directories = [str(PureWindowsPath(f'C:/MCP/mapping-{len(self.roots) + index}') / suffix)
                       for index in range(len(self.toolchains))
                       for suffix in ('', 'cmd', 'bin', 'usr/bin', 'mingw64/bin')]
        directories.extend([r'C:\Windows\System32', r'C:\Windows',
                            r'C:\Windows\System32\WindowsPowerShell\v1.0'])
        def quote(value):
            return "'" + value.replace("'", "''") + "'"
        prefix = ('$McpSpool=' + quote(self.guest_spool) + ';$McpCwd=' + quote(self.guest_cwd)
                  + ';$McpPath=' + quote(';'.join(directories)) + ';\n')
        git_roots = list(dict.fromkeys([*self.guest_roots.values(), self.guest_cwd]))
        prefix += '$McpGitRoots=@(' + ','.join(quote(p.replace('\\', '/')) for p in git_roots) + ');\n'
        script = prefix + Path(__file__).with_name('developer_guest.ps1').read_text(encoding='utf-8')
        encoded = base64.b64encode(script.encode('utf-16-le')).decode('ascii')
        ET.SubElement(ET.SubElement(config, 'LogonCommand'), 'Command').text = (
            'powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ' + encoded)
        return ET.tostring(config, encoding='unicode')

    def start(self, command: str) -> None:
        if self.thread:
            raise ValueError('VM_ALREADY_STARTED')
        # Full validation must finish before exposing any host map to the VM.
        configuration = self.configuration()
        self.files.mkdir(self.spool_name, self.root_id)
        self.spool_reader = FileReader(self.spool)
        self.thread = threading.Thread(target=self._run, args=(command, configuration), daemon=True)
        self.thread.start()

    def snapshot(self, name: str) -> bytes | None:
        path = self.spool / name
        if not os.path.lexists(path):
            return None
        try:
            checked = self.spool_reader.checked(name)
            value = self.files.read_bytes(self.spool_reader, checked)
        except ValueError as exc:
            if '讀取期間檔案已變更' in str(exc):
                return None
            raise
        if len(value) > MAX_OUTPUT_BYTES:
            raise ValueError('VM_OUTPUT_LIMIT')
        return value

    def collect_output(self) -> None:
        output = self.snapshot('output.txt')
        if output is None:
            return
        if len(output) < self.offset:
            raise ValueError('VM_OUTPUT_CHANGED')
        if len(output) > self.offset:
            self.stdout.queue.put(output[self.offset:])
            self.offset = len(output)

    def _run(self, command: str, configuration: str) -> None:
        owner = client = None
        exit_code = 1
        started = time.monotonic()
        try:
            with ExitStack() as locks:
                parents = {part for root in self.roots + self.toolchains for part in (root, *root.parents)}
                for parent in sorted(parents, key=lambda p: len(p.parts)):
                    locks.enter_context(locked_path(parent, directory=True))
                descriptor = os.environ.get(GUARD_ENV)
                if descriptor is None:
                    owner = GuardOwner(breakaway=True)
                    descriptor = owner.encode()
                client = GuardClient(descriptor)
                if self.cancelled.is_set():
                    return
                self.vm_id = client.request({'op': 'start', 'configuration': configuration})['vm_id']
                published = False
                while not self.cancelled.is_set():
                    if time.monotonic() - started > self.lifetime:
                        raise ValueError('VM_LIFETIME_LIMIT')
                    ready = self.snapshot('ready.json')
                    if ready is not None and not published:
                        if json.loads(ready) != {'ready': True}:
                            raise ValueError('VM_GUEST_PROTOCOL')
                        # Recheck source aliases after boot, before the first command.
                        from developer_control import build_vm_mapping_plan
                        build_vm_mapping_plan(self.roots, self.toolchains)
                        self.publish('request.json', {'command': command})
                        published = True
                    self.collect_output()
                    result = self.snapshot('result.json')
                    if result is not None:
                        result = json.loads(result)
                        if type(result.get('exit_code')) is not int:
                            raise ValueError('VM_GUEST_PROTOCOL')
                        exit_code = result['exit_code']
                        break
                    time.sleep(.1)
                client.request({'op': 'stop'})
                self.vm_id = None
                self.collect_output()
        except Exception as exc:
            self.error = str(exc) if isinstance(exc, ValueError) else 'VM_EXECUTION_FAILED'
            self.stdout.queue.put(('\n' + self.error + '\n').encode('utf-8'))
        finally:
            try:
                if client is not None:
                    if self.vm_id is not None:
                        client.request({'op': 'stop'})
                        self.vm_id = None
                    client.close()
                if owner is not None:
                    owner.close()
            except Exception:
                self.error = 'VM_STOP_UNCONFIRMED'
            self.code = exit_code
            self.stdout.queue.put(None)
            self.finished.set()

    def poll(self) -> int | None:
        return self.code if self.finished.is_set() else None

    def stop(self) -> None:
        self.cancelled.set()
        if not self.thread:
            return
        if not self.finished.wait(180) or self.error == 'VM_STOP_UNCONFIRMED':
            raise ValueError('VM_STOP_UNCONFIRMED：VM 停止未確認，禁止重連。')

    def close(self) -> None:
        self.stop()
        self.stdin.closed = True
        self.stdout.close()
        if self.spool_reader is not None and os.path.lexists(self.spool):
            original = self.spool_reader._root_identity
            info = self.spool.lstat()
            if linked(self.spool, info) or (info.st_dev, info.st_ino) != original:
                raise ValueError('VM_SPOOL_CHANGED：保留未知工作目錄，不自動刪除。')
            root = self.files.workspace.reader(self.root_id).root
            if not self.spool.resolve(strict=True).is_relative_to(root):
                raise ValueError('VM_SPOOL_CHANGED')
            # Python 3.8+ removes a Windows junction itself without traversing it.
            remove_owned_tree(self.spool)
