"""僅在暫存合成資料量測性能；不啟動通道、不修改電源。"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import tracemalloc
import statistics
from unittest.mock import patch


def instrument_measure(call):
    counters = {'read_bytes': 0, 'stat_calls': 0, 'lstat_calls': 0, 'direntry_stat_calls': 0}
    original_open, original_stat, original_lstat = Path.open, Path.stat, Path.lstat
    class Counted:
        def __init__(self, handle):
            self.handle = handle
        def __enter__(self):
            self.handle.__enter__()
            return self
        def __exit__(self, *args):
            return self.handle.__exit__(*args)
        def read(self, *args):
            value = self.handle.read(*args)
            counters['read_bytes'] += len(value)
            return value
        def __getattr__(self, name):
            return getattr(self.handle, name)
    def opened(path, *args, **kwargs):
        return Counted(original_open(path, *args, **kwargs))
    def stat(*args, **kwargs):
        counters['stat_calls'] += 1
        return original_stat(*args, **kwargs)
    def lstat(*args, **kwargs):
        counters['lstat_calls'] += 1
        return original_lstat(*args, **kwargs)
    original_entry_stat = os.DirEntry.stat
    def entry_stat(*args, **kwargs):
        counters['direntry_stat_calls'] += 1
        return original_entry_stat(*args, **kwargs)
    tracemalloc.start()
    start = time.perf_counter()
    with patch.object(os.DirEntry, 'stat', entry_stat), patch.object(Path, 'open', opened), patch.object(Path, 'stat', stat), patch.object(Path, 'lstat', lstat):
        result = call()
    elapsed = (time.perf_counter() - start) * 1000
    _, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()
    return result, dict(counters, elapsed_ms=round(elapsed, 3), python_peak_bytes=peak)


def run(source: Path, repeats=3, files=1000):
    import sys
    sys.path.insert(0, str(source.resolve()))
    from local_files_mcp import FileReader
    with tempfile.TemporaryDirectory(prefix='mcp-engineering-bench-') as directory:
        root = Path(directory)
        for i in range(files):
            (root / f'{i:04}.txt').write_bytes(b'marker\n')
        (root / 'large.txt').write_bytes(b'bounded line\n' * 150000)
        report = {'fixture_version': 1, 'files': files, 'repeats': repeats, 'instrumentation': 'tracemalloc and Python stat/read hooks; OS cache not flushed', 'samples': []}
        for _ in range(repeats):
            reader = FileReader(root, {'max_scan_files': 100000, 'max_scan_entries': 300000})
            page, first = instrument_measure(lambda: reader.list_files(limit=200))
            _, second = instrument_measure(lambda: reader.list_files(limit=200, cursor=page['next_cursor']))
            _, read = instrument_measure(lambda: reader.read_file('large.txt', 70000, 10))
            search_result, search = instrument_measure(lambda: reader.search_text('marker', limit=1))
            for name in ('scanned_entries', 'sorted_entries', 'skipped_entries'):
                first[name] = page[name]
            search['scanned_bytes'] = search_result['scanned_bytes']
            report['samples'].append({'first_page': first, 'second_page': second, 'read_window': read, 'search_limit': search})
        report['median'] = {name: {metric: statistics.median(sample[name][metric] for sample in report['samples'])
                            for metric in report['samples'][0][name]} for name in report['samples'][0]}
        if os.name == 'nt':
            from ctypes import wintypes
            class Counters(ctypes.Structure):
                _fields_ = [('cb', wintypes.DWORD), ('faults', wintypes.DWORD)] + [
                    (name, ctypes.c_size_t) for name in ('peak_working', 'working', 'peak_paged',
                    'paged', 'peak_nonpaged', 'nonpaged', 'pagefile', 'peak_pagefile')]
            kernel = ctypes.WinDLL('kernel32', use_last_error=True)
            kernel.GetCurrentProcess.restype = wintypes.HANDLE
            query = ctypes.WinDLL('psapi', use_last_error=True).GetProcessMemoryInfo
            query.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
            query.restype = wintypes.BOOL
            counters = Counters()
            counters.cb = ctypes.sizeof(counters)
            if query(kernel.GetCurrentProcess(), ctypes.byref(counters), counters.cb):
                report['process_lifetime_peak_working_set_bytes'] = counters.peak_working
            else:
                report['process_memory_status'] = 'API_FAILED'
        return report



def working_set():
    if os.name != 'nt':
        return None
    from ctypes import wintypes
    class Counters(ctypes.Structure):
        _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD)] + [
            (name, ctypes.c_size_t) for name in ('PeakWorkingSetSize', 'WorkingSetSize',
            'QuotaPeakPagedPoolUsage', 'QuotaPagedPoolUsage', 'QuotaPeakNonPagedPoolUsage',
            'QuotaNonPagedPoolUsage', 'PagefileUsage', 'PeakPagefileUsage')]
    counters = Counters()
    counters.cb = ctypes.sizeof(counters)
    kernel = ctypes.WinDLL('kernel32')
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    psapi = ctypes.WinDLL('psapi')
    psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(Counters), wintypes.DWORD]
    if not psapi.GetProcessMemoryInfo(kernel.GetCurrentProcess(), ctypes.byref(counters), counters.cb):
        return None
    return counters.PeakWorkingSetSize


async def measure_concurrency(reader, repeats=20):
    import asyncio
    from operation_budget import asynchronous, OperationError
    from benchmark_e2e import summarize
    call = asynchronous(reader.read_file_ranges)
    rows = []
    for count in (1, 4, 8):
        samples, busy, success = [], 0, 0
        for index in range(repeats + 1):
            started = time.perf_counter()
            results = await asyncio.gather(*[call('large.txt', [{'start_line': 1, 'line_count': 20}])
                                            for _ in range(count)], return_exceptions=True)
            samples.append((time.perf_counter() - started) * 1000)
            for result in results:
                if isinstance(result, OperationError) and str(result).startswith('RESOURCE_BUSY'):
                    busy += index > 0
                elif isinstance(result, BaseException):
                    raise result
                else:
                    success += index > 0
        rows.append(dict(concurrent=count, warm_ms=summarize(samples[1:]),
                         successful_calls=success, resource_busy_calls=busy))
    return dict(repeats=repeats, cases=rows, note='每批同時提交，無重試；每次完整驗證 16 MiB。')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', default='.')
    parser.add_argument('--files', type=int, default=1000)
    parser.add_argument('--repeats', type=int, default=10)
    parser.add_argument('--output', required=True)
    parser.add_argument('--legacy', action='store_true', help='保留原 fixture v1 與配置／I/O 計數量測')
    args = parser.parse_args()
    if not 1 <= args.files <= 50000 or args.repeats < 10:
        parser.error('files 1..50000，repeats 至少 10')
    if args.legacy:
        report = run(Path(args.source), repeats=args.repeats, files=args.files)
        Path(args.output).write_text(json.dumps(report, indent=2), encoding='utf-8')
        return 0
    sys.path.insert(0, str(Path(args.source).resolve()))
    from workspace_reader import WorkspaceReader
    from benchmark_e2e import summarize
    from operation_budget import Budget, operation
    import local_files_mcp
    from snapshot_cache import CACHE
    rows = []
    def measure(name, call):
        samples = []
        result = None
        for _ in range(args.repeats + 1):
            start = time.perf_counter()
            result = call()
            samples.append((time.perf_counter() - start) * 1000)
        row = dict(case=name, first_call_ms=samples[0], warm_ms=summarize(samples[1:]),
                   response_bytes=len(json.dumps(result, ensure_ascii=False, indent=2).encode()))
        if isinstance(result, dict):
            for key in ('scanned_entries', 'scanned_files', 'scanned_bytes', 'truncated'):
                if key in result:
                    row[key] = result[key]
        rows.append(row)
        print(json.dumps(row), flush=True)
    setup = time.perf_counter()
    with tempfile.TemporaryDirectory(prefix='mcp-performance-') as directory:
        root = Path(directory)
        for i in range(args.files):
            folder = root / f'd{i // 1000:03}'
            folder.mkdir(exist_ok=True)
            (folder / f'f{i:05}.txt').write_text('alpha beta gamma marker\n' * 80, encoding='utf-8')
        settings = dict(max_file_bytes=64 * 1024 * 1024, max_scan_files=100000,
                        max_scan_entries=300000, max_scan_bytes=1024 * 1024 * 1024)
        workspace = WorkspaceReader([{'id': 'main', 'path': str(root)}], 'main', settings)
        reader = workspace.reader(None)
        setup_ms = (time.perf_counter() - setup) * 1000
        measure('list_first', reader.list_files)
        first = reader.list_files(limit=20)
        measure('list_second', lambda: reader.list_files(limit=20, cursor=first['next_cursor']))
        measure('search_10_common', lambda: reader.search_texts(['alpha', 'beta', 'gamma', 'marker'] * 2 + ['alpha', 'beta']))
        measure('read_small', lambda: reader.read_file('d000/f00000.txt'))
        requests = [dict(path=f'd000/f{i:05}.txt', line_count=20) for i in range(32)]
        measure('read_32_equivalent', lambda: [workspace.read_files(requests[i:i+10]) for i in range(0,32,10)])
        if hasattr(reader, 'find_files'):
            # Walk uses a LIFO stack for directories; last directory is visited first.
            target = f'f{((args.files - 1) // 1000) * 1000:05}'
            measure('find_front', lambda: reader.find_files([target], limit_per_query=1))
            measure('find_absent', lambda: reader.find_files(['not-present']))
            measure('read_32', lambda: workspace.read_files(requests))
        with patch('local_files_mcp.checkpoint', wraps=local_files_mcp.checkpoint) as checked:
            reader.list_files()
            checkpoint_calls = checked.call_count
        data = (b'a' * 255 + b'\n') * 65536  # exactly 16 MiB
        (root / 'large.txt').write_bytes(data)
        ranges = [dict(start_line=n, line_count=20) for n in (1, 1000, 10000, 60000)]
        measure('four_reads_16mib', lambda: [reader.read_file('large.txt', **r) for r in ranges])
        io_metrics = {}
        if hasattr(reader, 'read_file_ranges'):
            measure('ranges_16mib', lambda: reader.read_file_ranges('large.txt', ranges))
            budget = Budget()
            with operation(budget), patch.object(reader, 'open_checked', wraps=reader.open_checked) as opened:
                reader.read_file_ranges('large.txt', ranges)
            io_metrics = dict(open_checked=opened.call_count, bytes_read=budget.bytes_read)
        _, instrumented = instrument_measure(lambda: reader.read_file('large.txt', 1, 20))
        report = dict(instrumented_read_16mib=instrumented, source=str(Path(args.source).resolve()), files=args.files, repeats=args.repeats,
                      setup_ms=setup_ms, cases=rows, checkpoint_calls=checkpoint_calls,
                      ranges_io=io_metrics, peak_working_set_bytes=working_set(),
                      note='合成 UTF-8 資料；同程序 warm calls；未清除 OS cache；不代表真實專案或遠端延遲。')
        Path(args.output).write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
        CACHE.items.clear()
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
