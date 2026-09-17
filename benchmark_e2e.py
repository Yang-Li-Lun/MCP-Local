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


async def compare_stdio(args):
    """新舊獨立連線逐次交錯；共用既有 cases 與 percentile 彙整。"""
    from contextlib import AsyncExitStack
    from tool_contract import contract
    baseline = Path(args.baseline_source).resolve()
    cases = json.loads(Path(args.cases).read_text(encoding='utf-8'))
    async with AsyncExitStack() as stack:
        sessions = {}
        for label, folder in [('baseline', baseline), ('v4', Path(__file__).resolve().parent)]:
            params = StdioServerParameters(command=sys.executable, args=[
                '-B', str(folder / 'local_files_mcp.py'), '--root', str(Path(args.root).resolve())])
            read, write = await stack.enter_async_context(stdio_client(params))
            session = await stack.enter_async_context(ClientSession(read, write))
            await session.initialize()
            expected = json.loads((folder / 'tool-contract.json').read_text(encoding='utf-8'))
            if contract((await session.list_tools()).tools) != expected:
                raise ValueError('基線或新版工具契約不符。')
            sessions[label] = session
        rows = []
        for case in cases:
            values = {label: [] for label in sessions}
            for index in range(args.repeats + 1):
                order = ['baseline', 'v4'] if index % 2 == 0 else ['v4', 'baseline']
                for label in order:
                    started = time.perf_counter()
                    result = await sessions[label].call_tool(case['tool'], case.get('arguments', {}))
                    if result.isError:
                        raise ValueError('交錯量測工具呼叫失敗。')
                    values[label].append((time.perf_counter() - started) * 1000)
            rows.append({'case': case['name'], **{label: {
                'first_call_ms': samples[0], 'warm_ms': summarize(samples[1:])}
                for label, samples in values.items()}})
        return {'cases': rows, 'comparison': '兩條 STDIO 連線逐次交錯，不並行執行工具。'}


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
    parser.add_argument('--baseline-source', help='STDIO 新舊交錯比較；來源須含相依模組及各自契約')
    args = parser.parse_args()
    if args.repeats < 20 or (args.mode == 'https' and not args.url):
        parser.error('至少重複 20 次；HTTPS 需要 URL')
    if args.baseline_source and args.mode != 'stdio':
        parser.error('--baseline-source 僅適用 STDIO')
    try:
        report = asyncio.run(compare_stdio(args) if args.baseline_source else run(args))
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
