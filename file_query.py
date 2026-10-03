"""完整 metadata 掃描後篩選排序；有界、短效、僅記憶體分頁快照。"""
from contextlib import contextmanager
import hashlib
import json
import math
import os
from pathlib import Path
import stat
from time import perf_counter
from typing import Any, Iterator

import format_reader
from incremental_state import hash_record, new_stats
from operation_budget import ACTIVE, bounded, checkpoint
from snapshot_cache import CACHE, SnapshotBuilder


SORT_FIELDS = ('name', 'path', 'size', 'created_time', 'modified_time', 'accessed_time')


def signature(info: os.stat_result) -> list:
    return [info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns,
            info.st_ctime_ns, getattr(info, 'st_birthtime_ns', None),
            info.st_mode, getattr(info, 'st_file_attributes', None), info.st_nlink]


def checked_stat(reader: Any, relative: str) -> tuple[Path, os.stat_result]:
    path = reader.checked(relative)
    info = path.lstat()
    if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or
            getattr(info, 'st_file_attributes', 0) & 0x402):
        raise ValueError('QUERY_CHANGED：檔案安全屬性已變更。')
    confirmed = reader.checked(relative).lstat()
    if signature(confirmed) != signature(info):
        raise ValueError('QUERY_CHANGED：檔案身分或 metadata 已變更。')
    return path, info


@contextmanager
def content_budget(reader: Any) -> Iterator[None]:
    budget = ACTIVE.get()
    original = budget.max_bytes
    budget.max_bytes = min(original, budget.bytes_read + reader.settings['max_scan_bytes'])
    try:
        yield
    finally:
        budget.max_bytes = original


@bounded
def query_files(reader: Any, *, directory: str = '.', name: str | None = None,
                extensions: list[str] | None = None, min_size: int | None = None,
                max_size: int | None = None, created_after: float | None = None,
                created_before: float | None = None, modified_after: float | None = None,
                modified_before: float | None = None, accessed_after: float | None = None,
                accessed_before: float | None = None, sort_by: str = 'path', order: str = 'asc',
                limit: int = 200, cursor: str | None = None, include_format: bool = False,
                include_capabilities: bool = False, include_sha256: bool = False) -> dict:
    started = perf_counter()
    if type(limit) is not int or not 1 <= limit <= 500:
        raise ValueError('QUERY_RANGE：limit 必須為 1 至 500。')
    if sort_by not in SORT_FIELDS or order not in ('asc', 'desc'):
        raise ValueError('QUERY_SORT：不支援的排序條件。')
    if any(type(v) is not bool for v in (include_format, include_capabilities, include_sha256)):
        raise ValueError('QUERY_OPTIONS：選項必須為布林值。')
    if name is not None and (not isinstance(name, str) or not 1 <= len(name) <= 200):
        raise ValueError('QUERY_NAME：名稱必須為 1 至 200 字元。')
    if extensions is not None:
        if (not isinstance(extensions, list) or not 1 <= len(extensions) <= 32 or
                any(not isinstance(v, str) or len(v) > 80 or
                    any(c in v for c in '/\\:*?"<>|\x00') for v in extensions)):
            raise ValueError('QUERY_EXTENSION：副檔名清單不合法。')
        extensions = sorted({'.' + v.lstrip('.').casefold() if v else '' for v in extensions})
    for value in (min_size, max_size):
        if value is not None and (type(value) is not int or not 0 <= value <= 2**53 - 1):
            raise ValueError('QUERY_SIZE：容量篩選不合法。')
    ranges = [('size', min_size, max_size), ('created_time', created_after, created_before),
              ('modified_time', modified_after, modified_before),
              ('accessed_time', accessed_after, accessed_before)]
    for field, low, high in ranges:
        if field != 'size' and any(v is not None and (type(v) not in (int, float) or
                                  not math.isfinite(v)) for v in (low, high)):
            raise ValueError('QUERY_TIME：時間必須是有限 Unix seconds UTC。')
        if low is not None and high is not None and low > high:
            raise ValueError('QUERY_RANGE：下限不可大於上限。')
    directory = reader.relative(reader.checked(directory))
    # Include all conditions except page size; tokens cannot cross root, policy or query.
    conditions = [directory, name, extensions, ranges, sort_by, order,
                  include_format, include_capabilities, include_sha256, reader.settings]
    fingerprint = hashlib.sha256(json.dumps(conditions, sort_keys=True).encode()).hexdigest()
    owner = reader._cursor_key.hex()
    if cursor is not None:
        token = reader._decode_cursor(cursor, 'query_files', directory)
        try:
            snapshot_id, offset_text = token.split('_')
            offset = int(offset_text)
            if offset < 0:
                raise ValueError()
        except (ValueError, TypeError):
            raise ValueError('QUERY_CURSOR：cursor 無效。') from None
        entry = CACHE.get(snapshot_id, owner, fingerprint)
    else:
        status = dict(truncated=False, skipped_entries=0)
        rows = SnapshotBuilder()
        for path, info in reader.walk(directory, status, _with_info=True, _strict_errors=True):
            checkpoint()
            # Refresh metadata after checking every ancestor; DirEntry is not read authorization.
            path, info = checked_stat(reader, reader.relative(path))
            row = format_reader.basic_metadata(reader, path, info)
            if name is not None and name.casefold() not in row['name'].casefold():
                continue
            if extensions is not None and row['extension'] not in extensions:
                continue
            if any((low is not None or high is not None) and row[field] is None
                   for field, low, high in ranges) or row[sort_by] is None:
                raise ValueError('QUERY_TIME_UNAVAILABLE：無法取得排序或篩選所需時間。')
            if any((low is not None and row[field] < low) or
                   (high is not None and row[field] > high) for field, low, high in ranges):
                continue
            if not rows.append(dict(path=row['path'], payload=json.dumps(row, ensure_ascii=False),
                                    signature=json.dumps(signature(info)))):
                raise ValueError('QUERY_SNAPSHOT_LIMIT：完整結果超過快照限制，請縮小範圍。')
        if status['truncated']:
            raise ValueError('QUERY_SCAN_LIMIT：掃描未完成，不能回報全域排序，請縮小範圍。')

        def key(item):
            row = json.loads(item['payload'])
            value = row.get(sort_by + '_ns')
            if value is None:
                value = row[sort_by]
            if isinstance(value, str):
                value = value.casefold()
            return value, row['path'].casefold(), row['path']

        rows.rows.sort(key=key, reverse=order == 'desc')
        checkpoint()
        snapshot_id = CACHE.put(owner, fingerprint, rows, status)
        entry = CACHE.get(snapshot_id, owner, fingerprint)
        offset = 0
    results = []
    stats = new_stats()
    with content_budget(reader):
        for item in entry['rows'][offset:offset + limit]:
            checkpoint()
            path, info = checked_stat(reader, item['path'])
            if signature(info) != json.loads(item['signature']):
                raise ValueError('QUERY_CHANGED：分頁檔案已變更，請從第一頁重新查詢。')
            row = json.loads(item['payload'])
            if include_format or include_capabilities:
                details = format_reader.file_info(reader, row['path'])
                if include_format:
                    for field in ('format', 'mime_type', 'detection', 'container_error'):
                        row[field] = details[field]
                if include_capabilities:
                    row['capabilities'] = details['capabilities']
            if include_sha256:
                row['sha256'] = hash_record(reader, path, stats, force=True)['sha256']
            _, final = checked_stat(reader, item['path'])
            if signature(final) != json.loads(item['signature']):
                raise ValueError('QUERY_CHANGED：讀取期間檔案已變更，請重新查詢。')
            results.append(row)
    end = offset + len(results)
    more = end < len(entry['rows'])
    return dict(files=results, returned_count=len(results), matched_count=len(entry['rows']),
                has_more=more, next_cursor=reader._encode_cursor('query_files', directory,
                snapshot_id + '_' + str(end)) if more else None,
                scan_complete=True, scan_truncated=False, truncated=more,
                skipped_entries=entry['status']['skipped_entries'],
                scanned_entries=entry['status'].get('scanned_entries', 0),
                metadata_snapshot=True, elapsed_ms=round((perf_counter() - started) * 1000, 3))
