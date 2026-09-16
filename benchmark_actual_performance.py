"""既有授權 Root，只量測名稱；不讀取內容、不變更正式設定。"""
import importlib.util
import json
from pathlib import Path
import statistics
import time
from connection_settings import load_settings
from local_files_mcp import FileReader
from snapshot_cache import CACHE

def main():
    spec = importlib.util.spec_from_file_location(
        'performance_original', 'archive/backups/performance-backup-20260913-180807/local_files_mcp.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    settings = load_settings()
    root = Path(settings['root'])
    report = {'repeats': 3, 'scope': 'existing default root',
              'instrumentation': 'wall clock only; no tracemalloc; OS cache not flushed'}
    for label, cls in [('before', module.FileReader), ('after', FileReader)]:
        samples = []
        for _ in range(3):
            reader = cls(root, settings['reader'])
            started = time.perf_counter()
            try:
                page = reader.list_files()
                sample = {k: page[k] for k in (
                    'scanned_entries', 'sorted_entries', 'skipped_entries',
                    'elapsed_ms', 'scan_truncated')}
                sample['wall_ms'] = (time.perf_counter() - started) * 1000
                if page['next_cursor']:
                    second = reader.list_files(cursor=page['next_cursor'])
                    sample['second_page_ms'] = second['elapsed_ms']
            except Exception as exc:
                sample = {'error_type': type(exc).__name__,
                          'wall_ms': (time.perf_counter() - started) * 1000}
            samples.append(sample)
            print(label, json.dumps(sample), flush=True)
        report[label] = samples
    Path('performance-actual.json').write_text(json.dumps(report, indent=2),encoding='utf-8')

if __name__ == '__main__':
    main()
