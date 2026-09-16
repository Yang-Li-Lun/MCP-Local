"""固定合成資料；每項分別量測時間、Python 配置峰值、I/O 與屬性查詢。"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import statistics
import tempfile
import time
import tracemalloc
from unittest.mock import patch


def measure(call):
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
            page, first = measure(lambda: reader.list_files(limit=200))
            _, second = measure(lambda: reader.list_files(limit=200, cursor=page['next_cursor']))
            _, read = measure(lambda: reader.read_file('large.txt', 70000, 10))
            search_result, search = measure(lambda: reader.search_text('marker', limit=1))
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


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--source', type=Path, default=Path(__file__).parent)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--files', type=int, default=1000)
    args = parser.parse_args()
    result = run(args.source, files=args.files)
    args.output.write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result['median']))
