"""真實 Windows Sandbox + MCP STDIO；所有資料均為獨立 fixture，工具鏈僅唯讀。"""
import argparse
import asyncio
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from tool_contract import SERVICE_VERSION, CONTRACT_VERSION, verify_contract
from developer_vm_guard import find_cli, invoke_cli
from developer_vm import remove_owned_tree

BOOTSTRAP = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import security_policy
security_policy.STATE_DIR = Path(sys.argv[2])
from local_files_mcp import main
sys.argv = ['local_files_mcp.py', '--root', sys.argv[3], '--access-mode', sys.argv[4], *sys.argv[5:]]
main()
'''


async def verify(toolchains: list[Path] | None = None) -> dict:
    if toolchains is None:
        codex = shutil.which('codex.exe')
        if not codex:
            raise ValueError('Acceptance requires the installed Codex CLI')
        toolchains = [Path(sys.base_prefix), Path(r'C:\Program Files\Git'),
                      Path(r'C:\Program Files\nodejs'), Path(codex).parent]
    executable = find_cli()
    if invoke_cli(executable, 'list')['WindowsSandboxEnvironments']:
        raise ValueError('A foreign VM exists; acceptance will not stop or reuse it')
    fixture = Path(tempfile.mkdtemp(prefix='mcp-dev-stdio-')).resolve()
    root, outside, state, readonly = (fixture / name for name in ('root', 'outside', 'state', 'readonly-tools'))
    for path in (root, outside, state, readonly):
        path.mkdir()
    sentinel = outside / 'sentinel.txt'
    sentinel.write_text('SYNTHETIC_OUTSIDE_SENTINEL', encoding='utf-8')
    (state / 'credential.txt').write_text('SYNTHETIC_CREDENTIAL', encoding='utf-8')
    (readonly / 'marker.txt').write_text('SYNTHETIC_READONLY_TOOL', encoding='utf-8')
    digest = hashlib.sha256(sentinel.read_bytes()).hexdigest()
    arguments = []
    for path in [*toolchains, readonly]:
        arguments.extend(['--developer-toolchain', str(path)])
    calls = 0
    results = {}
    last_session = None
    project = Path(__file__).resolve().parent

    def parameters(mode):
        return StdioServerParameters(command=sys.executable,
            args=['-I', '-B', '-c', BOOTSTRAP, str(project), str(state), str(root), mode,
                  *(arguments if mode == 'developer_control' else [])],
            env={'PYTHONUTF8': '1', 'CONTROL_PLANE_API_KEY': 'SYNTHETIC_API_KEY',
                 'TEST_SECRET_TOKEN': 'SYNTHETIC_SECRET'})

    try:
        async with stdio_client(parameters('developer_control')) as (read, write):
            async with ClientSession(read, write) as session:
                async def call(name, args=None, denied=False):
                    nonlocal calls
                    value = await session.call_tool(name, args or {})
                    calls += 1
                    assert bool(value.isError) == denied, (name, value.content)
                    return None if denied else json.loads(value.content[0].text)

                async def wait(identifier, marker=None, timeout=180):
                    deadline = time.monotonic() + timeout
                    while time.monotonic() < deadline:
                        output = await call('read_process_output', {'session_id': identifier, 'wait_ms': 1000})
                        if marker is not None and marker in output['output']:
                            return output
                        if output['completed']:
                            assert marker is None, output
                            assert output['exit_code'] == 0, output
                            assert output['vm_stopped'] is True, output
                            return output
                    raise AssertionError('Developer session timed out')

                initialized = await session.initialize()
                assert initialized.serverInfo.version == SERVICE_VERSION
                tools = (await session.list_tools()).tools
                verify_contract(tools, 'developer_control')
                assert len(tools) == 34
                diagnostics = await call('server_diagnostics')
                assert diagnostics['mode_ready'] and diagnostics['consistent']
                assert diagnostics['developer_backend']['execution_adapter_implemented']
                await call('set_access_mode', {'mode': 'host_control'}, denied=True)
                await call('start_process', {'command': 'ignored', 'developer_toolchain': str(outside)}, denied=True)
                await call('write_file', {'path': str(sentinel), 'content': 'denied'}, denied=True)
                await call('write_file', {'path': '../outside/denied.txt', 'content': 'denied'}, denied=True)
                await call('write_file', {'path': 'source.cs', 'content': 'class Hello { static void Main() { System.Console.WriteLine("COMPILED_OK"); } }'})
                await call('write_file', {'path': 'python-program.py', 'content': "from pathlib import Path\nPath('python-output.txt').write_text('PYTHON_FILE_OK')\n"})
                await call('write_file', {'path': 'node-program.js', 'content': "require('fs').writeFileSync('node-output.txt','NODE_FILE_OK');\n"})
                outside_path = str(sentinel).replace("'", "''")
                state_path = str(state / 'credential.txt').replace("'", "''")
                ro_guest = f'C:\\MCP\\mapping-{1 + len(toolchains)}\\marker.txt'
                program = r'''
$r=[ordered]@{}
$r.python=(& python -I -B -c 'import sys;print(sys.version)' | Out-String).Trim()
$r.node=(& node --version | Out-String).Trim()
$r.git=(& git --version | Out-String).Trim()
$r.codex=(& codex --version | Out-String).Trim()
Write-Output 'CHECK_TOOLS_DONE'
$r.stdin=[Console]::ReadLine()
$r.environment= -not ($env:CONTROL_PLANE_API_KEY -or $env:TEST_SECRET_TOKEN -or $env:MCP_LOCAL_VM_GUARD)
foreach($item in @(@('absolute','__OUTSIDE__'),@('parent','C:\MCP\mapping-0\..\outside\sentinel.txt'),@('credential','__STATE__'))) {
    try {[void][IO.File]::ReadAllText($item[1]);$r[$item[0]]='ALLOWED'} catch {$r[$item[0]]='DENIED'}
}
try {[IO.File]::WriteAllText('__OUTSIDE__','ESCAPED');$r.outside_write='ALLOWED'} catch {$r.outside_write='DENIED'}
$ErrorActionPreference='SilentlyContinue'
& cmd.exe /d /c 'type "__OUTSIDE__"' 2>$null | Out-Null
$r.cmd=if($LASTEXITCODE -eq 0){'ALLOWED'}else{'DENIED'}
& python -I -B -c 'import sys;open(sys.argv[1]).read()' '__OUTSIDE__' 2>$null | Out-Null
$r.python_child=if($LASTEXITCODE -eq 0){'ALLOWED'}else{'DENIED'}
try {[IO.File]::WriteAllText('__READONLY__','ESCAPED');$r.readonly='ALLOWED'} catch {$r.readonly='DENIED'}
& cmd.exe /d /c 'mklink /H "C:\MCP\mapping-0\readonly-link.txt" "__READONLY__"' 2>$null | Out-Null
$r.create_readonly_hardlink=if($LASTEXITCODE -eq 0){'ALLOWED'}else{'DENIED'}
if($r.create_readonly_hardlink -eq 'ALLOWED') {
    try {[IO.File]::WriteAllText('C:\MCP\mapping-0\readonly-link.txt','ESCAPED');$r.readonly_hardlink_write='ALLOWED'} catch {$r.readonly_hardlink_write='DENIED'}
} else {$r.readonly_hardlink_write='DENIED'}
& cmd.exe /d /c 'mklink /J "C:\MCP\mapping-0\readonly-junction" "__READONLY_DIR__"' 2>$null | Out-Null
try {[IO.File]::WriteAllText('C:\MCP\mapping-0\readonly-junction\marker.txt','ESCAPED');$r.readonly_junction_write='ALLOWED'} catch {$r.readonly_junction_write='DENIED'}
& cmd.exe /d /c 'mklink /H "C:\MCP\mapping-0\outside-link.txt" "__OUTSIDE__"' 2>$null | Out-Null
$r.create_external_hardlink=if($LASTEXITCODE -eq 0){'ALLOWED'}else{'DENIED'}
& cmd.exe /d /c 'mklink /J "C:\MCP\mapping-0\escape-junction" "__OUTSIDE_DIR__"' 2>$null | Out-Null
try {[void][IO.File]::ReadAllText('C:\MCP\mapping-0\escape-junction\sentinel.txt');$r.junction='ALLOWED'} catch {$r.junction='DENIED'}
$ErrorActionPreference='Stop'
[IO.File]::WriteAllText((Join-Path $env:MCP_WORKSPACE 'allowed.txt'),'VM_ROOT_WRITE')
& python -I -B python-program.py
if($LASTEXITCODE -ne 0){throw 'Python file operation failed'}
& node node-program.js
if($LASTEXITCODE -ne 0){throw 'Node file operation failed'}
& git -c init.defaultBranch=main init --quiet
if($LASTEXITCODE -ne 0){throw 'Git init failed'}
& git -c core.autocrlf=false add -- source.cs allowed.txt
if($LASTEXITCODE -ne 0){throw 'Git add failed'}
& git -c user.name=Fixture -c user.email=fixture@example.invalid commit --quiet -m fixture
if($LASTEXITCODE -ne 0){throw 'Git commit failed'}
$r.git_commit=(& git log -1 --format=%H | Out-String).Trim()
$compiler='C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe'
& $compiler /nologo /out:compiled.exe source.cs | Out-Null
if($LASTEXITCODE -ne 0){throw 'Compiler failed'}
$r.compiler=(& .\compiled.exe | Out-String).Trim()
Write-Output ('VM_REPORT:'+($r|ConvertTo-Json -Compress))
'''.replace('__OUTSIDE__', outside_path).replace('__OUTSIDE_DIR__', str(outside).replace("'", "''")).replace('__STATE__', state_path).replace('__READONLY__', ro_guest).replace('__READONLY_DIR__', ro_guest.rsplit('\\', 1)[0])
                started = await call('start_process', {'command': program, 'timeout_ms': 0, 'lifetime_seconds': 180})
                last_session = started['session_id']
                await call('interact_with_process', {'session_id': last_session, 'input': '中文🙂\n', 'close_stdin': True})
                output = await wait(last_session)
                lines = [line[len('VM_REPORT:'):] for line in output['output'].splitlines() if line.startswith('VM_REPORT:')]
                assert len(lines) == 1, output
                result = json.loads(lines[0])
                for name in ('absolute', 'parent', 'credential', 'outside_write', 'cmd', 'python_child', 'readonly', 'create_readonly_hardlink', 'readonly_hardlink_write', 'readonly_junction_write', 'create_external_hardlink', 'junction'):
                    assert result[name] == 'DENIED', result
                assert result['environment'] and result['stdin'] == '中文🙂', result
                assert '3.13' in result['python'] and result['node'].startswith('v') and result['git'].startswith('git version'), result
                assert 'codex' in result['codex'].lower() and result['compiler'] == 'COMPILED_OK', result
                assert (root / 'allowed.txt').read_text() == 'VM_ROOT_WRITE'
                assert (root / 'python-output.txt').read_text() == 'PYTHON_FILE_OK'
                assert (root / 'node-output.txt').read_text() == 'NODE_FILE_OK'
                assert len(result['git_commit']) == 40, result
                assert hashlib.sha256(sentinel.read_bytes()).hexdigest() == digest
                assert (readonly / 'marker.txt').read_text() == 'SYNTHETIC_READONLY_TOOL'
                await call('export_session_file', {'session_id': last_session, 'path': 'allowed.txt', 'destination': 'export.txt'})
                await call('close_session', {'session_id': last_session})
                await call('read_process_output', {'session_id': last_session}, denied=True)
                # Remove only the disposable fixture's junction itself, after VM teardown.
                for name in ('escape-junction', 'readonly-junction'):
                    junction = root / name
                    if os.path.lexists(junction):
                        assert junction.lstat().st_file_attributes & 0x400
                        os.rmdir(junction)
                results['execution_and_boundary'] = result
                # Leave a parent with a live child; closing STDIO must stop the VM.
                child = "import pathlib,time; p=pathlib.Path('heartbeat.txt');\nwhile True:p.write_text(str(time.time()));time.sleep(.2)"
                # A plain guest script avoids PowerShell/native argv quoting ambiguity.
                await call('write_file', {'path': 'heartbeat.py', 'content': child})
                heartbeat_command = "$child=Start-Process -FilePath (Get-Command python.exe).Source -ArgumentList @('-I','-B','heartbeat.py') -PassThru -NoNewWindow; Write-Output 'LIFECYCLE_READY'; Start-Sleep -Seconds 120"
                started = await call('start_process', {'command': heartbeat_command, 'timeout_ms': 0, 'lifetime_seconds': 180})
                last_session = started['session_id']
                await wait(last_session, marker='LIFECYCLE_READY')
                deadline = time.monotonic() + 10
                while not (root / 'heartbeat.txt').exists() and time.monotonic() < deadline:
                    await asyncio.sleep(.1)
                assert (root / 'heartbeat.txt').exists()
                stopped = await call('force_terminate', {'session_id': last_session})
                assert stopped['completed'] and stopped['vm_stopped'], stopped
                await call('interact_with_process', {'session_id': last_session, 'input': 'denied'}, denied=True)
                await call('close_session', {'session_id': last_session})
                (root / 'heartbeat.txt').unlink()
                results['force_terminate_vm'] = 'PASS'
                started = await call('start_process', {'command': heartbeat_command, 'timeout_ms': 0, 'lifetime_seconds': 180})
                last_session = started['session_id']
                await wait(last_session, marker='LIFECYCLE_READY')
                deadline = time.monotonic() + 10
                while not (root / 'heartbeat.txt').exists() and time.monotonic() < deadline:
                    await asyncio.sleep(.1)
                assert (root / 'heartbeat.txt').exists()
        deadline = time.monotonic() + 90
        while invoke_cli(executable, 'list')['WindowsSandboxEnvironments']:
            if time.monotonic() >= deadline:
                raise AssertionError('VM survived STDIO close')
            await asyncio.sleep(.5)
        before = (root / 'heartbeat.txt').read_bytes()
        await asyncio.sleep(1)
        assert (root / 'heartbeat.txt').read_bytes() == before
        results['stdio_parent_child_shutdown'] = 'PASS'
        async with stdio_client(parameters('read_only')) as (read, write):
            async with ClientSession(read, write) as session:
                await session.initialize()
                tools = (await session.list_tools()).tools
                verify_contract(tools)
                assert len(tools) == 22
                for name, args in [('start_process', {'command': 'denied'}), ('read_process_output', {'session_id': last_session}),
                                   ('write_file', {'path': 'denied.txt', 'content': 'denied'})]:
                    value = await session.call_tool(name, args)
                    calls += 1
                    assert value.isError
        results['downgrade_to_readonly'] = 'PASS'
        return {'status': 'PASS', 'service_version': SERVICE_VERSION, 'contract_version': CONTRACT_VERSION,
                'tools': [34, 22], 'calls': calls, 'transport': 'real_stdio', 'backend': 'real_windows_sandbox',
                'results': results, 'production': 'UNTOUCHED', 'remote_acceptance': 'NOT_RUN'}
    finally:
        # A failed cleanup retains the fixture; never remove a live VM's backing map.
        if not invoke_cli(executable, 'list')['WindowsSandboxEnvironments']:
            assert fixture.name.startswith('mcp-dev-stdio-') and fixture.parent == Path(tempfile.gettempdir()).resolve()
            remove_owned_tree(fixture)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--toolchain', action='append', type=Path)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    report = asyncio.run(verify(args.toolchain))
    if args.report:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
