"""三種公開模式 discovery 與受信任主機能力驗收；VM 執行明確記錄未就緒。"""
import argparse
import asyncio
import ctypes as c
from ctypes import wintypes as w
import json
from pathlib import Path
import sys
import tempfile

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from access_mode import HOST_TOOLS
from tool_contract import verify_contract, SERVICE_VERSION, CONTRACT_VERSION

BOOTSTRAP = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import security_policy
security_policy.STATE_DIR = Path(sys.argv[2])
import developer_control
developer_control.backend_status = lambda toolchains=None: {'ready': False, 'reason': 'FIXTURE_BACKEND_DISABLED', 'host_fallback': False}
import developer_toolchains
developer_toolchains.discover_toolchains = lambda roots=None: []
from local_files_mcp import main
sys.argv = ['local_files_mcp.py', '--root', sys.argv[3], '--access-mode', sys.argv[4]]
main()
'''


async def verify() -> dict:
    calls, results = 0, []
    kernel = c.WinDLL('kernel32', use_last_error=True)
    kernel.OpenProcess.argtypes, kernel.OpenProcess.restype = [w.DWORD, w.BOOL, w.DWORD], w.HANDLE
    kernel.WaitForSingleObject.argtypes, kernel.WaitForSingleObject.restype = [w.HANDLE, w.DWORD], w.DWORD
    kernel.CloseHandle.argtypes = [w.HANDLE]
    with tempfile.TemporaryDirectory(prefix='mcp-three-mode-stdio-') as temporary:
        base = Path(temporary).resolve()
        root, outside, state = base / 'authorized', base / 'outside', base / 'synthetic-state'
        for folder in (root, outside, state):
            folder.mkdir()
        (root / 'input.txt').write_text('fixture marker', encoding='utf-8')
        (state / 'credential.txt').write_text('SYNTHETIC_NOT_SECRET', encoding='utf-8')
        previous_session = '0' * 32
        for mode in ('read_only', 'developer_control', 'host_control', 'read_only'):
            owned_handles = []
            parameters = StdioServerParameters(command=sys.executable,
                args=['-I', '-B', '-c', BOOTSTRAP, str(Path(__file__).resolve().parent), str(state), str(root), mode],
                env={'CONTROL_PLANE_API_KEY': 'synthetic-only', 'TEST_SECRET_TOKEN': 'synthetic-only'})
            try:
                async with stdio_client(parameters) as (read, write):
                    async with ClientSession(read, write) as session:
                        async def call(name, arguments=None, *, denied=False, error_contains=None):
                            nonlocal calls
                            value = await session.call_tool(name, arguments or {})
                            calls += 1
                            assert bool(value.isError) == denied, (mode, name, value.content)
                            if denied:
                                if error_contains:
                                    assert error_contains in str(value.content), value.content
                                return None
                            return json.loads(value.content[0].text)
                        initialized = await session.initialize()
                        assert initialized.serverInfo.version == SERVICE_VERSION
                        tools = (await session.list_tools()).tools
                        verify_contract(tools, mode)
                        diagnostics = await call('server_diagnostics')
                        assert diagnostics['access_mode'] == mode and diagnostics['consistent']
                        assert diagnostics['contract_version'] == CONTRACT_VERSION
                        assert diagnostics['mode_ready'] == (mode != 'developer_control')
                        assert 'marker' in (await call('read_file', {'path': 'input.txt'}))['content']
                        await call('set_access_mode', {'mode': 'host_control'}, denied=True)
                        if mode == 'read_only':
                            for name in HOST_TOOLS:
                                await call(name, {'session_id': previous_session}, denied=True)
                        elif mode == 'developer_control':
                            await call('write_file', {'path': 'developer.txt', 'content': 'guarded root write'})
                            await call('write_file', {'path': str(outside / 'denied.txt'), 'content': 'denied'}, denied=True)
                            await call('start_process', {'command': "Set-Content denied.txt 'must not run'"},
                                       denied=True, error_contains='DEVELOPER_BACKEND_UNAVAILABLE')
                            assert not (root / 'denied.txt').exists()
                            assert (await call('list_sessions'))['sessions'] == []
                        else:
                            path = str(outside / 'written.txt')
                            created = await call('write_file', {'path': path, 'content': 'original'})
                            changed = await call('edit_block', {'path': path, 'old_string': 'original',
                                'new_string': 'updated', 'expected_sha256': created['sha256']})
                            assert 'updated' in (await call('read_host_file', {'path': path}))['content']
                            assert (await call('list_host_directory', {'directory': str(outside)}))['entries']
                            await call('create_directory', {'path': str(outside / 'new-directory')})
                            moved = str(root / 'moved.txt')
                            await call('move_file', {'source': path, 'destination': moved, 'expected_sha256': changed['sha256']})
                            await call('delete_file', {'path': moved, 'expected_sha256': changed['sha256']})
                            for name, arguments in (
                                ('read_host_file', {'path': str(state / 'credential.txt')}),
                                ('write_file', {'path': str(state / 'blocked.txt'), 'content': 'denied'})):
                                await call(name, arguments, denied=True)
                            state_path = str(state / 'credential.txt').replace("'", "''")
                            script = ("if ($env:CONTROL_PLANE_API_KEY -or $env:TEST_SECRET_TOKEN) {throw 'ENV_LEAK'}; "
                                      "if ([IO.File]::ReadAllText('" + state_path + "') -eq 'SYNTHETIC_NOT_SECRET') {Write-Output 'TRUSTED_COMMAND_SCOPE'}; "
                                      "Write-Output ('ECHO:'+[Console]::ReadLine()); "
                                      "[IO.File]::WriteAllText((Join-Path $env:MCP_WORKSPACE 'result.txt'),'host result')")
                            started = await call('start_process', {'command': script, 'working_directory': str(outside), 'timeout_ms': 0})
                            identifier = started['session_id']
                            assert started['sandbox'] == 'host_process'
                            await call('interact_with_process', {'session_id': identifier, 'input': 'hello\n', 'close_stdin': True})
                            output = await call('read_process_output', {'session_id': identifier, 'wait_ms': 3000})
                            assert output['completed'] and output['exit_code'] == 0, output
                            assert 'TRUSTED_COMMAND_SCOPE' in output['output'] and 'ECHO:hello' in output['output']
                            await call('export_session_file', {'session_id': identifier, 'path': 'result.txt', 'destination': 'exported.txt'})
                            assert (root / 'exported.txt').read_text() == 'host result'
                            assert not (await call('close_session', {'session_id': identifier}))['profile_removed']
                            waiting = await call('start_process', {'command': 'Start-Sleep -Seconds 60', 'timeout_ms': 0})
                            assert (await call('force_terminate', {'session_id': waiting['session_id']}))['completed']
                            await call('close_session', {'session_id': waiting['session_id']})
                            python = str(Path(sys.base_prefix) / 'python.exe').replace("'", "''")
                            script = ("$child=Start-Process -FilePath '" + python + "' -ArgumentList '-I','-B','-c','\"import time; time.sleep(120)\"' -PassThru -WindowStyle Hidden; "
                                      "Write-Output ('CHILD_PID='+[string]$child.Id); Start-Sleep -Seconds 120")
                            active = await call('start_process', {'command': script, 'timeout_ms': 1000})
                            previous_session = active['session_id']
                            if 'CHILD_PID=' not in active['output']:
                                active = await call('read_process_output', {'session_id': previous_session, 'wait_ms': 3000})
                            child_pid = int(next(line.split('=', 1)[1] for line in active['output'].splitlines() if line.startswith('CHILD_PID=')))
                            for pid in (active['pid'], child_pid):
                                handle = kernel.OpenProcess(0x100000, False, pid)
                                assert handle, 'unable to hold owned test-process identity'
                                owned_handles.append(handle)
                        results.append({'mode': mode, 'tools': len(tools), 'discovery': 'PASS',
                                        'mode_ready': diagnostics['mode_ready'],
                                        'vm_execution': 'FIXTURE_BACKEND_DISABLED' if mode == 'developer_control' else 'NOT_APPLICABLE'})
                for handle in owned_handles:
                    assert kernel.WaitForSingleObject(handle, 5000) == 0, 'owned host descendant survived STDIO shutdown'
                if owned_handles:
                    results[-1]['host_parent_and_child_shutdown'] = 'PASS'
            finally:
                for handle in owned_handles:
                    kernel.CloseHandle(handle)
    return {'service_version': SERVICE_VERSION, 'contract_version': CONTRACT_VERSION,
            'calls': calls, 'modes': results, 'transport': 'real_stdio',
            'host_trust_model': 'trusted_commands_not_credential_isolated',
            'developer_vm_acceptance': 'NOT_RUN_IN_THIS_SCRIPT', 'production': 'UNTOUCHED', 'remote_discovery': 'NOT_RUN'}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    report = asyncio.run(verify())
    encoded = json.dumps(report, ensure_ascii=False, indent=2)
    if args.report:
        args.report.write_text(encoded + '\n', encoding='utf-8')
    print(encoded)
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
