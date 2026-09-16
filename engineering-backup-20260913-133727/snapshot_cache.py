"""不保存內容的限量、限時記憶體列舉快照。"""
import threading
import time
import uuid
from collections import OrderedDict


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
            own = [key for key, value in self.items.items() if value['owner'] == owner]
            while len(own) >= 8:
                del self.items[own.pop(0)]
            while self.items and (len(self.items) >= 32 or
                    sum(len(e['rows']) for e in self.items.values()) + len(rows) > 250000):
                self.items.popitem(last=False)
            key = uuid.uuid4().hex
            self.items[key] = dict(owner=owner, fingerprint=fingerprint, rows=rows,
                                   status=dict(status), created=self.clock(), access=self.clock())
            return key

    def get(self, key: str, owner: str, fingerprint: str) -> dict:
        with self.lock:
            self._expire()
            entry = self.items.get(key)
            if not entry or entry['owner'] != owner or entry['fingerprint'] != fingerprint:
                raise ValueError('cursor 已失效，請從第一頁重新列出。')
            entry['access'] = self.clock()
            self.items.move_to_end(key)
            return entry


CACHE = SnapshotCache()
