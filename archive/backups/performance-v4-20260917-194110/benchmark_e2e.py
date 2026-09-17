"""量測明確案例的 local / STDIO / HTTPS；不輸出檔案內容或認證。"""
import argparse
import asyncio
import json
import math
import os
from pathlib import Path
import statistics
import sys
import time
from urllib.parse import urlsplit

import httpx
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.client.streamable_http import streamable_http_client
from tool_contract import verify_contract
from workspace_reader import WorkspaceReader


def summarize(samples):
    ordered = sorted(samples)
    return {'min': min(samples), 'p50': statistics.median(samples),
            'p90': ordered[math.ceil(len(samples) * .90) - 1],
            'p95': ordered[math.ceil(len(samples) * .95) - 1],
            'p99': ordered[math.ceil(len(samples) * .99) - 1], 'max': max(samples)}


async def measure(call, cases, repeats):
    rows = []
    for case in cases:
        samples = []
        for _ in range(repeats + 1):
            started = time.perf_counter()
            result = await call(case['tool'], case.get('arguments', {}))
            elapsed = (time.perf_counter() - started) * 1000
            if getattr(result, 'isError', False):
                raise ValueError('工具呼叫失敗')
            samples.append(elapsed)
        rows.append({'case': case['name'], 'first_call_ms': samples[0],
                     'warm_ms': summarize(samples[1:]), 'error_count': 0, 'disconnect_count': 0})
    return rows


async def run(args):
    cases = json.loads(Path(args.cases).read_text(encoding='utf-8'))
    started = time.perf_counter()
    if os.environ.get('MCP_LOCAL_BENCHMARK_POWER') and args.mode == 'local':
        from power_windows import WindowsPower
        WindowsPower().qos(os.environ['MCP_LOCAL_BENCHMARK_POWER'] == 'extreme')
    if args.mode == 'local':
        reader = WorkspaceReader([{'id': 'main', 'name': 'benchmark',
                                   'path': str(Path(args.root).resolve()), 'excluded_names': []}], 'main')
        async def call(name, arguments):
            return getattr(reader, name)(**arguments)
        return {'setup_ms': (time.perf_counter() - started) * 1000,
                'cases': await measure(call, cases, args.repeats)}
    async def session_results(read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            verify_contract((await session.list_tools()).tools)
            setup = (time.perf_counter() - started) * 1000
            return {'setup_ms': setup, 'cases': await measure(session.call_tool, cases, args.repeats)}
    if args.mode == 'stdio':
        params = StdioServerParameters(command=sys.executable, args=[
            '-B', str(Path(__file__).with_name('local_files_mcp.py')), '--root', str(Path(args.root).resolve())])
        channel = os.environ.get('MCP_LOCAL_BENCHMARK_CHANNEL')
        if channel:
            params.args.extend(['--power-channel', channel, '--power-owner', os.environ['MCP_LOCAL_BENCHMARK_OWNER']])
        async with stdio_client(params) as (read, write):
            return await session_results(read, write)
    url = urlsplit(args.url)
    if url.scheme != 'https' or url.username or url.password or url.query or url.fragment:
        raise ValueError('需使用不含認證的 HTTPS URL')
    token = os.environ.get('MCP_BENCHMARK_BEARER')
    headers = {'Authorization': 'Bearer ' + token} if token else {}
    async with httpx.AsyncClient(headers=headers, timeout=60) as client:
        async with streamable_http_client(args.url, http_client=client) as (read, write, _):
            return await session_results(read, write)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode', choices=['local', 'stdio', 'https'], required=True)
    parser.add_argument('--root', default='shared')
    parser.add_argument('--url')
    parser.add_argument('--cases', required=True, help='僅使用明確授權路徑的案例 JSON')
    parser.add_argument('--repeats', type=int, default=20)
    parser.add_argument('--output', required=True)
    args = parser.parse_args()
    if args.repeats < 20 or (args.mode == 'https' and not args.url):
        parser.error('至少重複 20 次；HTTPS 需要 URL')
    try:
        report = asyncio.run(run(args))
    except Exception:
        print('BENCHMARK_FAILED：請檢查案例、端點及認證；未輸出內容或原始例外。')
        return 1
    report.update(mode=args.mode, repeats=args.repeats,
                  note='同一連線，首呼叫與暖連線分列；未清除 OS cache；不代表最終用戶端延遲。')
    Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    print('BENCHMARK_OK')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
