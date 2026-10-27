"""既有授權 Root，只量測名稱；不讀取內容、不變更正式設定。"""

# Direct script execution and python -m use the same installed package.
if __package__ in (None, ""):
    import sys
    from pathlib import Path
    sys.path.insert(0, str(Path(__file__).resolve().parents[2]))

import importlib.util
import importlib
import argparse
from datetime import datetime
import json
from pathlib import Path
import statistics
import time
import sys
from unittest.mock import patch
from mcp_local.connection_settings import load_settings
from mcp_local.local_files_mcp import FileReader
from mcp_local.snapshot_cache import CACHE

def load_original_module():
    project_root = Path(__file__).resolve().parents[2]
    snapshot = (project_root / 'archive/backups/performance-backup-20260913-180807'
                / 'local_files_mcp.py').resolve(strict=True)
    if not snapshot.is_relative_to(project_root) or not snapshot.is_file():
        raise ValueError('Original performance snapshot must be a repository file')
    spec = importlib.util.spec_from_file_location('performance_original', snapshot)
    if spec is None or spec.loader is None:
        raise ImportError(f'Cannot load original performance snapshot: {snapshot}')
    module = importlib.util.module_from_spec(spec)
    # This fixed archived file imports the former top-level dependency names.
    # Bind them to the same current modules for the duration of loading only.
    dependencies = {name: importlib.import_module('mcp_local.' + name) for name in (
        'operation_budget', 'security_policy', 'stream_read', 'snapshot_cache', 'stream_search', 'reader_settings')}
    with patch.dict(sys.modules, dependencies):
        spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=(Path(__file__).resolve().parents[2]
                        / 'reports/performance/runs' / datetime.now().strftime('actual-%Y%m%d-%H%M%S-%f.json')))
    args = parser.parse_args()
    module = load_original_module()
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
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open('x', encoding='utf-8') as output:
        output.write(json.dumps(report, indent=2))

if __name__ == '__main__':
    main()
