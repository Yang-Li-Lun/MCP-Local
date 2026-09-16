"""建立並清理專用暫存資料，輸出分頁、搜尋效能與 Python 記憶體峰值。"""
import argparse
import json
from pathlib import Path
import tempfile
import time
import tracemalloc
from unittest.mock import patch
from local_files_mcp import FileReader
from workspace_reader import WorkspaceReader


def measured(call):
    tracemalloc.start()
    start = time.perf_counter()
    result = call()
    elapsed = (time.perf_counter() - start) * 1000
    _, peak = tracemalloc.get_traced_memory()
    tracemalloc.stop()
    return result, {'elapsed_ms': round(elapsed, 2), 'python_peak_bytes': peak}


def benchmark(count: int) -> dict:
    with tempfile.TemporaryDirectory(prefix='mcp-benchmark-') as directory:
        root = Path(directory)
        for i in range(count):
            (root / f'{i:06}.txt').write_text('fixture marker\n', encoding='utf-8')
        reader = FileReader(root, {'max_scan_files': count, 'max_scan_entries': count + 1})
        report = {'entries': count}
        with patch.object(reader, 'walk', wraps=reader.walk) as walk:
            page, report['first_page'] = measured(lambda: reader.list_files(limit=200))
            if page['next_cursor']:
                page, report['second_page'] = measured(lambda: reader.list_files(limit=200, cursor=page['next_cursor']))
            start = time.perf_counter()
            while page['next_cursor']:
                page = reader.list_files(limit=200, cursor=page['next_cursor'])
            report['remaining_pages_ms'] = round((time.perf_counter() - start) * 1000, 2)
            report['main_scans'] = walk.call_count
        _, report['list_directory'] = measured(reader.list_directory)
        result, report['search_text'] = measured(lambda: reader.search_text('absent'))
        report['scanned_bytes'] = result['scanned_bytes']
        workspace = WorkspaceReader([{'id': 'fixture', 'path': str(root)}], 'fixture')
        _, report['project_context'] = measured(workspace.project_context)
        _, report['read_files'] = measured(lambda: workspace.read_files([{'path': '000000.txt'}]))
        return report


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--entries', type=int, choices=(1000, 10000, 50000, 100000), default=1000)
    parser.add_argument('--soak-seconds', type=int, default=0, help='每分鐘重測；8 小時為 28800')
    args = parser.parse_args()
    deadline = time.monotonic() + max(0, args.soak_seconds)
    while True:
        print(json.dumps(benchmark(args.entries)), flush=True)
        if time.monotonic() >= deadline:
            return
        time.sleep(min(60, max(0, deadline - time.monotonic())))


if __name__ == '__main__':
    main()
