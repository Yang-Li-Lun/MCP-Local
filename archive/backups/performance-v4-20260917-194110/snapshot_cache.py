"""不保存內容的限量、限時記憶體列舉快照。"""
from types import MappingProxyType
import copy
import sys
import threading
import time
import uuid
from collections import OrderedDict


SNAPSHOT_BYTES = 16 * 1024 * 1024
CACHE_BYTES = 64 * 1024 * 1024


def estimated_bytes(value):
    if isinstance(value, dict):
        return sys.getsizeof(value) + sum(estimated_bytes(k) + estimated_bytes(v) for k, v in value.items())
    if isinstance(value, (list, tuple)):
        return sys.getsizeof(value) + sum(estimated_bytes(v) for v in value)
    return sys.getsizeof(value)


class SnapshotCache:
    def __init__(self, clock=time.monotonic):
        self.clock = clock
        self.items = OrderedDict()
        self.lock = threading.RLock()

    def _expire(self) -> None:
        now = self.clock()
        for key, entry in list(self.items.items()):
            if now - entry['access'] >= 60 or now - entry['created'] >= 300:
                del self.items[key]

    def put(self, owner: str, fingerprint: str, rows: list, status: dict) -> str:
        with self.lock:
            self._expire()
            if len(rows) > 100000:
                rows = rows[:100000]
                status = dict(status, truncated=True)
            bounded_rows = []
            size = 1024
            for row in rows:
                cost = estimated_bytes(row) + 16
                if size + cost > SNAPSHOT_BYTES:
                    status = dict(status, truncated=True, truncation_reason='SNAPSHOT_BYTES')
                    break
                bounded_rows.append(copy.deepcopy(row))
                size += cost
            rows = tuple(MappingProxyType(row) if isinstance(row, dict) else row for row in bounded_rows)
            own = [key for key, value in self.items.items() if value['owner'] == owner]
            while len(own) >= 8:
                del self.items[own.pop(0)]
            while self.items and (len(self.items) >= 32 or
                    sum(len(e['rows']) for e in self.items.values()) + len(rows) > 250000 or
                    sum(e['estimated_bytes'] for e in self.items.values()) + size > CACHE_BYTES):
                self.items.popitem(last=False)
            key = uuid.uuid4().hex
            self.items[key] = dict(owner=owner, fingerprint=fingerprint, rows=rows,
                                   estimated_bytes=size, status=dict(status), created=self.clock(), access=self.clock())
            return key

    def get(self, key: str, owner: str, fingerprint: str) -> dict:
        with self.lock:
            self._expire()
            entry = self.items.get(key)
            if not entry or entry['owner'] != owner or entry['fingerprint'] != fingerprint:
                raise ValueError('cursor 已失效，請從第一頁重新列出。')
            entry['access'] = self.clock()
            self.items.move_to_end(key)
            return dict(entry, status=dict(entry['status']))


CACHE = SnapshotCache()
