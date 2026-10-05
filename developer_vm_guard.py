"""獨立 VM 生命週期程序；只執行固定 Windows Sandbox CLI，不執行使用者命令。"""
from __future__ import annotations

import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import threading
import uuid
from multiprocessing.connection import Listener

MAX_MESSAGE = 128 * 1024


def find_cli() -> Path:
    """只接受 Microsoft 已註冊套件內的 CLI，不執行 PATH 中的同名程式。"""
    windows = Path(os.environ['SystemRoot'])
    from developer_vm_client import guard_environment
    module = str(windows / 'System32/WindowsPowerShell/v1.0/Modules/Appx/Appx.psd1').replace("'", "''")
    query = ("[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); "
             "Import-Module -Name '" + module + "' -ErrorAction Stop; "
             "Appx\\Get-AppxPackage -Name MicrosoftWindows.WindowsSandbox | "
             "Select-Object PackageFamilyName,InstallLocation,@{Name='ProgramFiles';Expression={[Environment]::GetFolderPath('ProgramFiles')}} | ConvertTo-Json -Compress")
    result = subprocess.run([str(windows / 'System32/WindowsPowerShell/v1.0/powershell.exe'),
                             '-NoLogo', '-NoProfile', '-NonInteractive', '-Command', query],
                            capture_output=True, timeout=20, creationflags=subprocess.CREATE_NO_WINDOW,
                            cwd=Path(__file__).resolve().parent, env=guard_environment())
    if result.returncode:
        raise ValueError('VM_CLI_UNAVAILABLE')
    package = json.loads(result.stdout.decode('utf-8-sig'))
    if not isinstance(package, dict) or package.get('PackageFamilyName') != 'MicrosoftWindows.WindowsSandbox_cw5n1h2txyewy':
        raise ValueError('VM_CLI_UNAVAILABLE')
    directory = Path(package['InstallLocation']).resolve(strict=True)
    expected = Path(package['ProgramFiles']) / 'WindowsApps'
    if not directory.is_relative_to(expected.absolute()):
        raise ValueError('VM_CLI_UNTRUSTED_PATH')
    executable = directory / 'wsb.exe'
    if not executable.is_file():
        raise ValueError('VM_CLI_UNAVAILABLE')
    return executable


def invoke_cli(executable: Path, *args: str) -> dict:
    arguments = [str(executable), *args, '--raw']
    if len(subprocess.list2cmdline(arguments).encode('utf-16-le')) // 2 > 30000:
        raise ValueError('VM_CONFIG_LIMIT')
    result = subprocess.run(arguments, capture_output=True, timeout=120,
                            creationflags=subprocess.CREATE_NO_WINDOW)
    if result.returncode:
        raise ValueError('VM_CLI_FAILED')
    raw = result.stdout.decode('utf-8-sig').strip()
    value = json.loads(raw) if raw else {}
    if not isinstance(value, dict):
        raise ValueError('VM_CLI_PROTOCOL')
    return value


class Lifecycle:
    def __init__(self, executable: Path):
        self.executable = executable
        self.identifier: str | None = None
        self.connector = None

    def ids(self) -> set[str]:
        result = invoke_cli(self.executable, 'list')
        rows = result.get('WindowsSandboxEnvironments')
        if not isinstance(rows, list) or any(not isinstance(row, dict) or 'Id' not in row for row in rows):
            raise ValueError('VM_CLI_PROTOCOL')
        return {str(uuid.UUID(row['Id'])) for row in rows}

    def start(self, configuration: str, cancelled: threading.Event) -> dict:
        if self.identifier is not None or self.ids():
            raise ValueError('VM_BUSY：已有 VM，禁止接管其他 Sandbox。')
        self.identifier = str(uuid.uuid4())
        result = invoke_cli(self.executable, 'start', '--id', self.identifier, '--config', configuration)
        if result.get('Id', '').casefold() != self.identifier or cancelled.is_set():
            self.stop()
            raise ValueError('VM_START_CANCELLED')
        self.connector = subprocess.Popen(
            [str(self.executable), 'connect', '--id', self.identifier, '--raw'],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW)
        if cancelled.is_set():
            self.stop()
            raise ValueError('VM_START_CANCELLED')
        return {'vm_id': self.identifier}

    def stop(self) -> None:
        if self.identifier is None:
            return
        identifier = self.identifier
        # A CLI error never proves absence. Preserve ownership until read-back.
        if identifier in self.ids():
            invoke_cli(self.executable, 'stop', '--id', identifier)
        if identifier in self.ids():
            raise ValueError('VM_STOP_UNCONFIRMED')
        self.identifier = None
        if self.connector is not None:
            try:
                self.connector.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.connector.kill()
                self.connector.wait(timeout=5)
            self.connector = None


def serve(settings: dict, source, output) -> None:
    from host_windows import HostProcess
    identity = HostProcess(Path(__file__).resolve().parent)
    identity.close()
    address, key = settings['address'], bytes.fromhex(settings['authkey'])
    if (len(key) != 32 or not address.startswith('\\\\.\\pipe\\MCP-Developer-')
            or len(address) != len('\\\\.\\pipe\\MCP-Developer-') + 32):
        raise ValueError('VM_GUARD_CONFIGURATION')
    uuid.UUID(address[-32:])
    lifecycle = Lifecycle(find_cli())
    cancelled = threading.Event()
    requests = queue.Queue(maxsize=8)
    connections = []
    listener = Listener(address, family='AF_PIPE', authkey=key)

    def monitor_owner():
        # No secret or command is expected after the initial configuration.
        source.read(1)
        cancelled.set()

    def accept():
        while not cancelled.is_set():
            try:
                connection = listener.accept()
                connections.append(connection)
                while not cancelled.is_set():
                    message = json.loads(connection.recv_bytes(MAX_MESSAGE).decode('utf-8'))
                    requests.put((connection, message))
                break
            except (EOFError, OSError, ValueError):
                requests.put((None, {'op': 'disconnect'}))
            finally:
                if connections:
                    connections.pop().close()

    threading.Thread(target=monitor_owner, daemon=True).start()
    threading.Thread(target=accept, daemon=True).start()
    output.write(json.dumps({'ready': True}) + '\n')
    output.flush()
    try:
        while not cancelled.is_set():
            try:
                connection, message = requests.get(timeout=.1)
            except queue.Empty:
                continue
            try:
                if not isinstance(message, dict):
                    raise ValueError('VM_GUARD_PROTOCOL')
                if set(message) == {'op', 'configuration'} and message['op'] == 'start':
                    configuration = message['configuration']
                    if not isinstance(configuration, str) or len(configuration) > 28000:
                        raise ValueError('VM_CONFIG_LIMIT')
                    value = lifecycle.start(configuration, cancelled)
                elif set(message) == {'op'} and message['op'] in ('stop', 'disconnect'):
                    lifecycle.stop()
                    value = {'stopped': True}
                else:
                    raise ValueError('VM_GUARD_PROTOCOL')
                reply = {'ok': True, **value}
            except Exception:
                reply = {'ok': False, 'error': 'VM_OPERATION_FAILED'}
            if connection is not None:
                try:
                    connection.send_bytes(json.dumps(reply).encode('utf-8'))
                except (EOFError, OSError):
                    lifecycle.stop()
    finally:
        # Failure propagates to the local parent, which must block reconnect.
        lifecycle.stop()
        listener.close()
        for connection in connections:
            connection.close()
    output.write(json.dumps({'stopped': True}) + '\n')
    output.flush()


def main() -> int:
    try:
        line = sys.stdin.readline(MAX_MESSAGE + 1)
        if len(line) > MAX_MESSAGE:
            raise ValueError('VM_GUARD_PROTOCOL')
        serve(json.loads(line), sys.stdin, sys.stdout)
        return 0
    except Exception:
        print(json.dumps({'ok': False, 'error': 'VM_GUARD_FAILED'}), flush=True)
        return 1


if __name__ == '__main__':
    # -I excludes user site packages, PYTHONPATH and the caller's working directory.
    # Only this protected service directory is reintroduced for sibling modules.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    raise SystemExit(main())
