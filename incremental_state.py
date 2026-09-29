"""Bounded SHA-256 inventories and process-local incremental baselines."""
import codecs
import hashlib
import json
import os
import re
import threading
import time
import uuid
from collections import OrderedDict
from pathlib import Path

from operation_budget import checkpoint, ensure_output_limit
from snapshot_cache import estimated_bytes

MAX_HASH_FILES = 32
MAX_SNAPSHOT_ENTRIES = 10000
SNAPSHOT_BYTES = 16 * 1024 * 1024
CACHE_BYTES = 64 * 1024 * 1024
MAX_BASELINES = 32
MAX_OWNER_BASELINES = 8
BASELINE_TTL_SECONDS = 24 * 60 * 60
MAX_CHANGE_ITEMS = 500


def signature(info):
    return (info.st_dev, info.st_ino, info.st_size, info.st_mtime_ns,
            info.st_ctime_ns, info.st_nlink, getattr(info, 'st_file_attributes', 0))


def handle_signature(handle):
    info = signature(os.fstat(handle.fileno()))
    if os.name != 'nt':
        return info
    # Windows st_ctime is creation time on supported Python 3.10, not ChangeTime.
    # Query the already checked handle so same-size writes with restored mtime
    # still invalidate metadata reuse. Failure is closed, never a weak fallback.
    import ctypes
    import msvcrt
    from ctypes import wintypes

    class FileBasicInfo(ctypes.Structure):
        _fields_ = [('CreationTime', ctypes.c_longlong),
                    ('LastAccessTime', ctypes.c_longlong),
                    ('LastWriteTime', ctypes.c_longlong),
                    ('ChangeTime', ctypes.c_longlong),
                    ('FileAttributes', wintypes.DWORD)]

    query = ctypes.WinDLL('kernel32', use_last_error=True).GetFileInformationByHandleEx
    query.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    query.restype = wintypes.BOOL
    basic = FileBasicInfo()
    if not query(msvcrt.get_osfhandle(handle.fileno()), 0, ctypes.byref(basic), ctypes.sizeof(basic)):
        raise ValueError('HASH_METADATA_UNAVAILABLE：無法安全查驗檔案變更時間。')
    return info + (basic.ChangeTime,)


class Baselines:
    """Opaque, owner/policy/scope-bound tokens; never persists source data."""
    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.lock = threading.RLock()
        self.items = OrderedDict()

    def expire(self):
        for token, value in list(self.items.items()):
            if self.clock() - value['created'] >= BASELINE_TTL_SECONDS:
                del self.items[token]

    def get(self, token, owner, scope):
        if not isinstance(token, str) or not re.fullmatch('[0-9a-f]{32}', token):
            raise ValueError('BASELINE_INVALID：基準代號格式錯誤。')
        with self.lock:
            self.expire()
            value = self.items.get(token)
            if value is None or value['owner'] != owner or value['scope'] != scope:
                raise ValueError('BASELINE_UNAVAILABLE：基準已過期、重啟、被淘汰或範圍不符，請建立新基準。')
            return value['rows']

    def put(self, owner, scope, rows):
        size = estimated_bytes(rows)
        if size > SNAPSHOT_BYTES or len(rows) > MAX_SNAPSHOT_ENTRIES:
            raise ValueError('SNAPSHOT_LIMIT：請縮小專案範圍。')
        checkpoint()
        with self.lock:
            self.expire()
            own = [key for key, value in self.items.items() if value['owner'] == owner]
            while len(own) >= MAX_OWNER_BASELINES:
                del self.items[own.pop(0)]
            while self.items and (len(self.items) >= MAX_BASELINES or
                    sum(v['size'] for v in self.items.values()) + size > CACHE_BYTES):
                self.items.popitem(last=False)
            token = uuid.uuid4().hex
            self.items[token] = dict(owner=owner, scope=scope, rows=rows,
                                     size=size, created=self.clock())
            return token


BASELINES = Baselines()


def scope_key(reader, directory):
    # Internal only: paths are hashed and never returned to the MCP client.
    return hashlib.sha256(json.dumps([str(reader.root), reader._root_identity,
        directory, reader.settings], sort_keys=True).encode()).hexdigest()


def hash_record(reader, path, stats, previous=None, force=False):
    """Checked-open even on reuse; cached hashes never authorize text reads."""
    with reader.open_checked(path) as handle:
        before = handle_signature(handle)
        if not force and previous and previous.get('signature') == before:
            stats['reused_hashes'] += 1
            result = dict(previous)
        else:
            decoder = codecs.getincrementaldecoder('utf-8-sig')()
            digest = hashlib.sha256()
            used = 0
            while True:
                checkpoint()
                chunk = handle.read(min(65536, reader.settings['max_file_bytes'] + 1 - used,
                                        reader.settings['max_scan_bytes'] + 1 - stats['bytes_read']))
                if not chunk:
                    break
                used += len(chunk)
                stats['bytes_read'] += len(chunk)
                checkpoint(bytes_read=len(chunk))
                if (used > reader.settings['max_file_bytes'] or
                        stats['bytes_read'] > reader.settings['max_scan_bytes']):
                    raise ValueError('HASH_BYTES_LIMIT：已達讀取容量上限，請縮小範圍。')
                if b'\x00' in chunk:
                    raise ValueError('HASH_FORMAT：只支援 UTF-8 文字與程式碼。')
                try:
                    decoder.decode(chunk)
                except UnicodeError:
                    raise ValueError('HASH_FORMAT：只支援 UTF-8 文字與程式碼。') from None
                digest.update(chunk)
            try:
                decoder.decode(b'', final=True)
            except UnicodeError:
                raise ValueError('HASH_FORMAT：只支援 UTF-8 文字與程式碼。') from None
            if handle_signature(handle) != before:
                raise ValueError('SCAN_CHANGED：檔案在讀取期間變更，請重試。')
            stats['hashed_files'] += 1
            result = dict(type='file', signature=before, sha256=digest.hexdigest(), size=used)
    if signature(reader.checked(reader.relative(path)).stat()) != before[:7]:
        raise ValueError('SCAN_CHANGED：檔案身分或屬性已變更，請重試。')
    return result


def new_stats():
    return dict(bytes_read=0, hashed_files=0, reused_hashes=0, scanned_entries=0, skipped_entries=0)


def inventory(reader, relative, stats, previous=None, force=False):
    start = reader.checked(relative)
    rows = {}
    size = 1024
    files = 0
    directory_signatures = {}

    def add(path, info):
        nonlocal size, files
        checkpoint()
        checked = reader.checked(reader.relative(path))
        key = '.' if path == start else path.relative_to(start).as_posix()
        if checked.is_dir():
            row = dict(type='directory')
            directory_signatures[reader.relative(checked)] = signature(checked.stat())
        else:
            files += 1
            if files > reader.settings['max_scan_files']:
                raise ValueError('SCAN_LIMIT：請縮小範圍。')
            row = hash_record(reader, checked, stats, (previous or {}).get(key), force)
        size += estimated_bytes(key) + estimated_bytes(row) + 128
        if len(rows) >= MAX_SNAPSHOT_ENTRIES or size > SNAPSHOT_BYTES:
            raise ValueError('SNAPSHOT_LIMIT：請縮小範圍。')
        rows[key] = row

    if start.is_file():
        add(start, start.stat())
    else:
        add(start, start.stat())
        pending = [start]
        while pending:
            folder = pending.pop()
            status = dict(truncated=False, skipped_entries=0)
            for path, info in reader.walk(reader.relative(folder), status, True,
                                           _with_info=True, _strict_errors=True):
                add(path, info)
                if rows[path.relative_to(start).as_posix()]['type'] == 'directory':
                    pending.append(path)
            stats['scanned_entries'] += status.get('scanned_entries', 0)
            stats['skipped_entries'] += status['skipped_entries']
            if status['truncated'] or stats['scanned_entries'] > reader.settings['max_scan_entries']:
                raise ValueError('SCAN_LIMIT：清冊不完整，請縮小範圍。')
    # A bounded scan is not an atomic filesystem snapshot. Detect observed races.
    for relative_path, before in directory_signatures.items():
        checkpoint()
        if signature(reader.checked(relative_path).stat()) != before:
            raise ValueError('SCAN_CHANGED：目錄在掃描期間變更，請重試。')
    for key, row in rows.items():
        checkpoint()
        path = start if key == '.' else start / key
        current = reader.checked(reader.relative(path))
        if row['type'] == 'file' and signature(current.stat()) != row['signature'][:7]:
            raise ValueError('SCAN_CHANGED：檔案在掃描期間變更，請重試。')
    return rows


def validate_previous(reader, directory, previous, current):
    """Never expose an old path or mislabel revoked/hidden/unreadable as deleted."""
    base = reader.checked(directory)
    for key in previous.keys() - current.keys():
        checkpoint()
        try:
            reader.checked(reader.relative(base if key == '.' else base / key))
        except FileNotFoundError:
            continue
        except (ValueError, OSError):
            raise ValueError('BASELINE_SCOPE_CHANGED：先前項目已無法安全驗證，請建立新基準。') from None
        raise ValueError('SCAN_INCOMPLETE：先前項目未被完整列舉，請建立新基準。')


def difference(left, right):
    counts = dict(added=0, deleted=0, modified=0, same=0)
    changes = {key: [] for key in counts}
    returned = 0
    # Changed entries first so unchanged names cannot crowd out useful changes.
    classified = []
    for key in sorted(left.keys() | right.keys()):
        checkpoint()
        a, b = left.get(key), right.get(key)
        kind = 'added' if a is None else 'deleted' if b is None else 'modified' if (
            a['type'] != b['type'] or a.get('sha256') != b.get('sha256')) else 'same'
        counts[kind] += 1
        classified.append((kind, key, (b or a)['type']))
    for kind, key, entry_type in sorted(classified, key=lambda row: row[0] == 'same'):
        if returned >= MAX_CHANGE_ITEMS:
            break
        changes[kind].append(dict(path=key, type=entry_type))
        returned += 1
    return dict(counts=counts, changes=changes, returned_count=returned,
                details_truncated=sum(counts.values()) > returned)


def hash_files(reader, paths):
    if (type(paths) is not list or not 1 <= len(paths) <= MAX_HASH_FILES or
            any(type(path) is not str for path in paths)):
        raise ValueError('paths 必須包含 1 至 32 個相對檔案路徑。')
    checked = [reader.checked(path) for path in paths]
    stats = new_stats()
    records = []
    for path in checked:
        record = hash_record(reader, path, stats, force=True)
        records.append(dict(path=reader.relative(path), sha256=record['sha256'], size=record['size']))
    return dict(algorithm='SHA-256', files=records, **stats)


def compare_paths(left_reader, right_reader, left, right):
    left_stats, right_stats = new_stats(), new_stats()
    a = inventory(left_reader, left, left_stats, force=True)
    b = inventory(right_reader, right, right_stats, force=True)
    return dict(**difference(a, b), left_stats=left_stats, right_stats=right_stats,
                comparison='raw_bytes_sha256', scope='allowed_utf8_files_and_directories')


def project_status(reader, owner, directory, baseline_id, force_hash):
    if type(force_hash) is not bool:
        raise ValueError('force_hash 必須是布林值。')
    base = reader.checked(directory)
    if not base.is_dir():
        raise ValueError('專案路徑必須為相對目錄。')
    directory = reader.relative(base)
    scope = scope_key(reader, directory)
    previous = BASELINES.get(baseline_id, owner, scope) if baseline_id is not None else None
    stats = new_stats()
    rows = inventory(reader, directory, stats, previous, force_hash)
    if previous is not None:
        validate_previous(reader, directory, previous, rows)
    result = dict(**difference(previous if previous is not None else rows, rows), **stats,
                  baseline_created=previous is None, compared_to=baseline_id,
                  cache_mode='full_hash' if force_hash else 'checked_metadata',
                  atomic_snapshot=False, expires_after_seconds=BASELINE_TTL_SECONDS,
                  scope='allowed_utf8_files_and_directories')
    ensure_output_limit(dict(result, baseline_id='0' * 32))
    checkpoint()
    result['baseline_id'] = BASELINES.put(owner, scope, rows)
    return result
