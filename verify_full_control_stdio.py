"""隔離 fixture 的真實 MCP 雙模式驗收，不載入正式設定、金鑰或遠端通道。"""
import argparse
import asyncio
import hashlib
import json
from pathlib import Path
import sys
import tempfile

from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from access_mode import CONTROL_TOOLS
from tool_contract import SERVICE_VERSION, CONTRACT_VERSION, verify_contract


async def verify() -> dict:
    results = []
    calls = 0
    with tempfile.TemporaryDirectory(prefix='mcp-mode-stdio-') as temporary:
        root = Path(temporary).resolve()
        (root / 'input.txt').write_text('原始 marker\r\n', encoding='utf-8', newline='')
        (root / '.hidden.txt').write_text('hidden fixture')
        sentinel = root / 'host-only.txt'
        sentinel.write_text('host-canary-not-secret')
        for mode in ('read_only', 'full_control', 'read_only'):
            lifecycle_workspace = None
            parameters = StdioServerParameters(command=sys.executable, args=[
                '-B', str(Path(__file__).with_name('local_files_mcp.py')), '--root', str(root), '--access-mode', mode],
                env={'CONTROL_PLANE_API_KEY': 'fixture-not-real', 'TEST_SECRET_TOKEN': 'fixture-only'})
            async with stdio_client(parameters) as (read, write):
                async with ClientSession(read, write) as session:
                    async def call(name, arguments=None, *, denied=False):
                        nonlocal calls
                        result = await session.call_tool(name, arguments or {})
                        calls += 1
                        if bool(result.isError) != denied:
                            raise AssertionError(f'{mode}/{name}: unexpected isError={result.isError}: {result.content}')
                        return None if denied else json.loads(result.content[0].text)
                    initialized = await session.initialize()
                    assert initialized.serverInfo.version == SERVICE_VERSION
                    tools = (await session.list_tools()).tools
                    verify_contract(tools, mode)
                    diagnostics = await call('server_diagnostics')
                    assert diagnostics['consistent'] and diagnostics['access_mode'] == mode
                    assert diagnostics['contract_version'] == CONTRACT_VERSION
                    assert 'marker' in (await call('read_file', {'path': 'input.txt'}))['content']
                    assert (await call('search_text', {'query': 'marker'}))['matches']
                    if mode == 'read_only':
                        for name in CONTROL_TOOLS:
                            await call(name, {}, denied=True)
                        results.append({'mode': mode, 'tools': len(tools), 'mutation_and_execution_tools': 'rejected'})
                        continue
                    await call('create_directory', {'path': 'work'})
                    created = await call('write_file', {'path': 'work/edit.txt', 'content': 'alpha\r\n中文\r\n'})
                    await call('write_file', {'path': 'work/edit.txt', 'content': 'overwrite'}, denied=True)
                    changed = await call('edit_block', {'path': 'work/edit.txt', 'old_string': 'alpha\n中文',
                        'new_string': 'beta\n文字', 'expected_sha256': created['sha256']})
                    assert '文字' in (await call('read_file', {'path': 'work/edit.txt'}))['content']
                    assert (await call('search_text', {'query': 'beta', 'directory': 'work'}))['matches']
                    await call('edit_block', {'path': 'work/edit.txt', 'old_string': 'beta',
                        'new_string': 'invalid', 'expected_sha256': created['sha256']}, denied=True)
                    for path in ('../escape.txt', '.hidden.txt', 'secrets.json', 'NUL.txt', 'file.txt:stream'):
                        await call('write_file', {'path': path, 'content': 'denied'}, denied=True)
                    await call('write_file', {'path': 'extra.txt', 'content': 'denied', 'extra': True}, denied=True)
                    await call('move_file', {'source': 'work/edit.txt', 'destination': 'work/moved.txt',
                                           'expected_sha256': changed['sha256']})
                    await call('file_info', {'path': 'work/moved.txt'})
                    host_path = str(sentinel).replace("'", "''")
                    script = ("Get-Content input.txt; if ($env:CONTROL_PLANE_API_KEY -or $env:TEST_SECRET_TOKEN) {throw 'ENV_LEAK'}; "
                        "try {[IO.File]::ReadAllText('" + host_path + "') | Out-Null; Write-Output 'ESCAPE'} catch {Write-Output 'HOST_DENIED'}; "
                        "Write-Output 'READY'; $line=[Console]::ReadLine(); Write-Output ('ECHO:'+$line); "
                        "[IO.File]::WriteAllText((Join-Path $env:MCP_WORKSPACE 'output.txt'),'匯出 marker')")
                    started = await call('start_process', {'command': script, 'import_paths': ['input.txt'], 'timeout_ms': 0})
                    identifier = started['session_id']
                    assert not started['completed']
                    assert (await call('list_sessions'))['sessions']
                    await call('export_session_file', {'session_id': identifier, 'path': 'output.txt', 'destination': 'output.txt'}, denied=True)
                    await call('interact_with_process', {'session_id': identifier, 'input': 'hello\n', 'close_stdin': True})
                    output = await call('read_process_output', {'session_id': identifier, 'wait_ms': 3000})
                    assert output['completed'] and output['exit_code'] == 0, output
                    assert all(value in output['output'] for value in ('原始 marker', 'HOST_DENIED', 'ECHO:hello')), output
                    assert 'ESCAPE' not in output['output'] and 'host-canary-not-secret' not in output['output']
                    await call('export_session_file', {'session_id': identifier, 'path': 'output.txt', 'destination': 'output.txt'})
                    assert (root / 'output.txt').read_text(encoding='utf-8') == '匯出 marker'
                    await call('close_session', {'session_id': identifier})
                    await call('read_process_output', {'session_id': identifier}, denied=True)
                    waiting = await call('start_process', {'command': 'Start-Sleep -Seconds 60', 'timeout_ms': 0})
                    stopped = await call('force_terminate', {'session_id': waiting['session_id']})
                    assert stopped['completed']
                    await call('close_session', {'session_id': waiting['session_id']})
                    await call('delete_file', {'path': 'work/moved.txt', 'expected_sha256': changed['sha256']})
                    assert not (root / 'work/moved.txt').exists()
                    assert sentinel.read_text() == 'host-canary-not-secret'
                    lifecycle = await call('start_process', {
                        'command': "Write-Output $env:MCP_WORKSPACE; Start-Sleep -Seconds 60", 'timeout_ms': 1000})
                    lifecycle_workspace = Path(lifecycle['output'].strip())
                    assert lifecycle_workspace.is_absolute() and lifecycle_workspace.exists(), lifecycle
                    results.append({'mode': mode, 'tools': len(tools), 'file_read_write_edit_move_delete': 'PASS',
                        'search_metadata': 'PASS', 'powershell_session_stdin_output': 'PASS',
                        'appcontainer_host_and_environment_denial': 'PASS', 'explicit_export': 'PASS'})
            if lifecycle_workspace:
                assert not lifecycle_workspace.exists(), 'STDIO shutdown did not clean up active sandbox'
                results[-1]['active_session_shutdown_cleanup'] = 'PASS'
    return {'service_version': SERVICE_VERSION, 'contract_version': CONTRACT_VERSION,
            'modes': results, 'calls': calls, 'transport': 'real_stdio',
            'remote_secure_tunnel': 'NOT_RUN', 'production_settings': 'UNTOUCHED'}


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
