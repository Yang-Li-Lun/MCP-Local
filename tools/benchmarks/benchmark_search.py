"""合成 fixture 的搜尋基準；不載入正式設定，不連線遠端，不更動電源。"""

# Direct script execution and python -m use the same installed package.
if __package__ in (None, ""):
    import sys
    from pathlib import Path
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import argparse
import asyncio
from contextlib import ExitStack
import hashlib
import io
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tarfile
import tempfile
import time
import tracemalloc
from typing import Any, Callable
from unittest.mock import patch
from tools.benchmarks.source_loader import source_module


PROJECT = Path(__file__).resolve().parents[2]
CASES = ('query_name', 'query_all', 'query_pages', 'workflow')


def digest(value: Any) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, ensure_ascii=False).encode()).hexdigest()


def fixture(root: Path, count: int) -> None:
    for number in range(count):
        path = root / 'inventory' / f'group{number % 12:02}' / 'src' / f'file{number:04}.txt'
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('synthetic inventory\n' * (number % 11 + 1), encoding='utf-8')
        os.utime(path, (1100000000 + number, 1200000000 + number))
    (root / 'task').mkdir()
    for label in ('a', 'b', 'c'):
        lines = [f'ordinary line {number}' for number in range(1, 2001)]
        for line in (50, 1500):
            lines[line - 1] = f'marker_{label} evidence {line}'
        (root / 'task' / f'module_{label}.py').write_text('\n'.join(lines) + '\n', encoding='utf-8')


def source_snapshot(ref: str, destination: Path) -> str:
    """只取指定 Git revision 的執行 Python、guest 腳本與契約。"""
    sha = subprocess.check_output(['git', 'rev-parse', '--verify', ref + '^{commit}'], cwd=PROJECT, text=True).strip()
    names = subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', sha], cwd=PROJECT, text=True).splitlines()
    names = [name for name in names if
             ('/' not in name and (name.endswith('.py') or name.startswith('tool-contract') and name.endswith('.json')))
             or name.startswith('mcp_local/') and name.endswith(('.py', '.ps1'))
             or name.startswith('contracts/tool-contract') and name.endswith('.json')]
    raw = subprocess.check_output(['git', 'archive', sha, '--', *names], cwd=PROJECT)
    with tarfile.open(fileobj=io.BytesIO(raw)) as archive:
        for member in archive:
            target = destination / member.name
            if (member.name not in names or not member.isfile() or '\\' in member.name
                    or not target.resolve().is_relative_to(destination.resolve())):
                raise ValueError('Unexpected baseline member')
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(archive.extractfile(member).read())
    return sha


async def workload(call: Callable, case: str, plan: str, count: int) -> Any:
    if case.startswith('query_'):
        options = dict(directory='inventory', limit=10)
        if case == 'query_name':
            options['name'] = f'file{count - 1:04}'
        elif case == 'query_all':
            options.update(sort_by='size', order='desc')
        else:
            options['limit'] = 500
        rows = []
        while True:
            page = await call('query_files', options)
            assert page['scan_complete'] and not page['scan_truncated']
            assert page['matched_count'] == (1 if case == 'query_name' else count)
            # OS access times may change due to reads; validate them in functional tests.
            rows.extend({k: v for k, v in row.items() if k not in
                         ('accessed_time', 'accessed_time_ns')} for row in page['files'])
            if case != 'query_pages' or not page['has_more']:
                break
            options['cursor'] = page['next_cursor']
        if case == 'query_pages':
            assert len(rows) == count and len({r['path'] for r in rows}) == count
        return rows
    names = [f'module_{label}' for label in ('a', 'b', 'c')]
    terms = [f'marker_{label}' for label in ('a', 'b', 'c')]
    located, matches = [], []
    for queries in ([names] if plan == 'batch' else [[name] for name in names]):
        result = await call('find_files', dict(queries=queries, directory='task'))
        assert not result['truncated']
        located.extend(row['path'] for group in result['results'] for row in group['matches'])
    if plan == 'batch':
        result = await call('search_texts', dict(queries=terms, directory='task'))
        assert not result['truncated']
        matches = [row for group in result['results'] for row in group['matches']]
    else:
        for term in terms:
            result = await call('search_text', dict(query=term, directory='task'))
            assert not result['truncated']
            matches.extend(result['matches'])
    assert sorted(set(row['path'] for row in matches)) == sorted(located)
    assert len(matches) == 6
    windows = {}
    for match in matches:
        windows.setdefault(match['path'], []).append(dict(start_line=match['line'] - 1, line_count=3))
    evidence = []
    for path, ranges in sorted(windows.items()):
        if plan == 'batch':
            result = await call('read_file_ranges', dict(path=path, ranges=ranges))
            assert not result['truncated']
            evidence.extend((path, row['start_line'], row['content']) for row in result['ranges'])
        else:
            for window in ranges:
                result = await call('read_file', dict(path=path, **window))
                evidence.append((path, result['start_line'], result['text'] if 'text' in result else result['content']))
    return dict(paths=sorted(located), matches=sorted(matches, key=lambda r: (r['path'], r['line'])),
                evidence=sorted(evidence))


async def measure(invoke: Callable, case: str, plan: str, count: int, wire: bool = False) -> dict:
    calls, response_bytes, service_ms = 0, 0, 0

    async def call(name, arguments):
        nonlocal calls, response_bytes, service_ms
        result = await invoke(name, arguments)
        calls += 1
        if wire:
            assert not result.isError, str(result.content)
            response_bytes += len(result.model_dump_json(by_alias=True, exclude_none=True).encode())
            result = json.loads(result.content[0].text)
        else:
            response_bytes += len(json.dumps(result, ensure_ascii=False, indent=2).encode())
        service_ms += result.get('elapsed_ms', 0)
        return result

    started, cpu = time.perf_counter(), time.process_time()
    value = await workload(call, case, plan, count)
    row = dict(wall_ms=(time.perf_counter() - started) * 1000,
               process_cpu_ms=(time.process_time() - cpu) * 1000,
               tool_calls=calls, response_bytes=response_bytes, result_sha256=digest(value))
    if case.startswith('query_'):
        row['query_service_elapsed_ms'] = service_ms
    return row


async def worker(args: argparse.Namespace) -> dict:
    sys.path.insert(0, str(args.source))
    WorkspaceReader = source_module(args.source, 'workspace_reader').WorkspaceReader
    CACHE = source_module(args.source, 'snapshot_cache').CACHE
    reader = WorkspaceReader([dict(id='fixture', path=str(args.fixture))], 'fixture')

    async def direct(name, arguments):
        return getattr(reader, name)(**arguments)

    results = {}
    if args.transport == 'stdio':
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        bootstrap = ('import sys; sys.path.insert(0, sys.argv.pop(1)); '
                     'from local_files_mcp import main; main()')
        params = StdioServerParameters(command=sys.executable, args=[
            '-I', '-B', '-c', bootstrap, str(args.source), '--root', str(args.fixture)])
        async with stdio_client(params) as (read, write):
            async with ClientSession(read, write) as session:
                await session.initialize()
                for case in CASES:
                    results[case] = await measure(session.call_tool, case, args.plan, args.files, True)
    else:
        format_reader = source_module(args.source, 'format_reader')
        budget = source_module(args.source, 'operation_budget')
        Budget, operation = budget.Budget, budget.operation
        internal = reader.reader(None)
        for case in CASES:
            CACHE.items.clear()
            if not args.resources:
                results[case] = await measure(direct, case, args.plan, args.files)
                continue
            # Instrumentation is a separate run and is never used as timing evidence.
            budget = Budget()
            counts = {}

            def counted(label, function):
                counts[label] = 0

                def invoke(*values, **options):
                    counts[label] += 1
                    return function(*values, **options)

                return invoke

            with ExitStack() as stack:
                for name in ('checked', 'walk', 'open_checked'):
                    stack.enter_context(patch.object(internal, name, counted(name + '_calls', getattr(internal, name))))
                stack.enter_context(patch.object(format_reader, 'basic_metadata',
                    counted('metadata_calls', format_reader.basic_metadata)))
                tracemalloc.start()
                try:
                    with operation(budget):
                        row = await measure(direct, case, args.plan, args.files)
                    _, peak = tracemalloc.get_traced_memory()
                finally:
                    tracemalloc.stop()
                results[case] = dict(result_sha256=row['result_sha256'], tool_calls=row['tool_calls'],
                    python_peak_bytes=peak, validated_content_bytes=budget.bytes_read,
                    scanned_entries=budget.entries, **counts)
    return results


def manifest(folder: Path) -> dict:
    paths = list(folder.glob('*.py')) + list(folder.glob('tool-contract*.json'))
    paths += list((folder / 'mcp_local').rglob('*.py')) + list((folder / 'contracts').glob('tool-contract*.json'))
    return {p.relative_to(folder).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(paths)}


def run(args: argparse.Namespace) -> None:
    args.output_dir.mkdir(parents=True, exist_ok=False)
    temporary_base = PROJECT / '.test-tmp'
    temporary_base.mkdir(exist_ok=True)
    if not temporary_base.resolve().is_relative_to(PROJECT.resolve()):
        raise ValueError('Temporary directory must remain within the repository')
    report = dict(baseline_ref=args.baseline_ref, python=sys.version, files=args.files, repeats=args.repeats,
        method='Same synthetic fixture, one discarded warmup per arm; alternating AB/BA; no OS cache flush; startup excluded.',
        cpu_scope='direct: service process; stdio: client process only, not server CPU',
        resource_scope='separate direct instrumentation run; Python traced peak, not native RSS',
        response_scope='direct: pretty JSON payload; stdio: CallToolResult JSON, excluding JSON-RPC envelope',
        equivalence='All metadata except OS access times; all paths, search hits and requested line windows.',
        workflow='before: 3 find_files + 3 search_text + 6 read_file; after: find_files + search_texts + 3 read_file_ranges. Both plans use pre-existing tools.',
        remote_acceptance='NOT_RUN', runs=[], resources={})
    with tempfile.TemporaryDirectory(prefix='search-benchmark-', dir=temporary_base) as temporary:
        base = Path(temporary).resolve()
        if not base.is_relative_to(temporary_base.resolve()):
            raise ValueError('Unexpected temporary directory')
        baseline, root = base / 'baseline', base / 'fixture'
        baseline.mkdir()
        root.mkdir()
        report['baseline_sha'] = source_snapshot(args.baseline_ref, baseline)
        report['source_hashes'] = dict(before=manifest(baseline), after=manifest(PROJECT))
        fixture(root, args.files)
        before = {p.relative_to(root).as_posix(): [hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_mtime_ns]
                  for p in root.rglob('*') if p.is_file()}
        report['fixture_sha256'] = digest(before)

        def launch(arm, transport, resources=False):
            command = [sys.executable, '-B', str(Path(__file__).resolve()), '--source',
                str(baseline if arm == 'before' else PROJECT), '--fixture', str(root),
                '--plan', 'serial' if arm == 'before' else 'batch', '--transport', transport,
                '--files', str(args.files)] + (['--resources'] if resources else [])
            completed = subprocess.run(command, capture_output=True, text=True, encoding='utf-8', timeout=240)
            if completed.returncode:
                raise RuntimeError(completed.stderr[-6000:])
            return json.loads(completed.stdout)

        for transport in ('direct', 'stdio'):
            for repetition in range(-1, args.repeats):
                arms = ('before', 'after') if repetition % 2 else ('after', 'before')
                for arm in arms:
                    samples = launch(arm, transport)
                    print(f'{transport} {arm} {repetition + 1}/{args.repeats}', flush=True)
                    if repetition >= 0:
                        report['runs'].append(dict(transport=transport, arm=arm, repetition=repetition, cases=samples))
                        (args.output_dir / 'comparison.json').write_text(
                            json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
        for arm in ('before', 'after'):
            report['resources'][arm] = launch(arm, 'direct', True)
        after = {p.relative_to(root).as_posix(): [hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_mtime_ns]
                 for p in root.rglob('*') if p.is_file()}
        assert before == after, 'Fixture contents or modification times changed'
        report['fixture_unchanged'] = True
        for case in CASES:
            hashes = {row['cases'][case]['result_sha256'] for row in report['runs']}
            hashes.update(resource[case]['result_sha256'] for resource in report['resources'].values())
            assert len(hashes) == 1, ('Result mismatch', case)
        report['equivalent_results'] = True
    report['summary'] = {}
    for transport in ('direct', 'stdio'):
        report['summary'][transport] = {}
        for case in CASES:
            report['summary'][transport][case] = {}
            for arm in ('before', 'after'):
                rows = [row['cases'][case] for row in report['runs'] if row['transport'] == transport and row['arm'] == arm]
                report['summary'][transport][case][arm] = {
                    key: statistics.median(row[key] for row in rows)
                    for key in rows[0] if key != 'result_sha256'}
    (args.output_dir / 'comparison.json').write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report['summary'], ensure_ascii=False, indent=2))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline-ref')
    parser.add_argument('--output-dir', type=Path)
    parser.add_argument('--files', type=int, default=1200)
    parser.add_argument('--repeats', type=int, default=5)
    parser.add_argument('--source', type=Path)
    parser.add_argument('--fixture', type=Path)
    parser.add_argument('--plan', choices=('serial', 'batch'), default='batch')
    parser.add_argument('--transport', choices=('direct', 'stdio'), default='direct')
    parser.add_argument('--resources', action='store_true')
    args = parser.parse_args()
    if not 501 <= args.files <= 2400 or not 1 <= args.repeats <= 10:
        parser.error('files: 501..2400; repeats: 1..10 (within unchanged reader limits)')
    if args.source:
        print(json.dumps(asyncio.run(worker(args)), ensure_ascii=False))
    else:
        if not args.baseline_ref or not args.output_dir:
            parser.error('--baseline-ref and a new --output-dir are required')
        run(args)


if __name__ == '__main__':
    main()
