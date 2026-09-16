"""有限工作名額、單調期限及合作式取消；不保證中斷阻塞中的核心呼叫。"""
import asyncio
import contextvars
import functools
import json
import threading
import time
from contextlib import contextmanager

MAX_WORKERS = 4
TIMEOUT_SECONDS = 30
ACTIVE = contextvars.ContextVar('reader_operation', default=None)
SLOTS = threading.BoundedSemaphore(MAX_WORKERS)


class OperationError(RuntimeError):
    pass


class Budget:
    def __init__(self, timeout=TIMEOUT_SECONDS, clock=time.monotonic):
        self.clock = clock
        self.deadline = clock() + timeout
        self.cancel = threading.Event()
        self.bytes_read = 0
        self.entries = 0
        self.max_bytes = 1024 * 1024 * 1024
        self.max_entries = 300000

    def check(self):
        if self.cancel.is_set():
            raise OperationError('OPERATION_CANCELLED：操作已取消。')
        if self.bytes_read > self.max_bytes or self.entries > self.max_entries:
            raise OperationError('OPERATION_RESOURCE_LIMIT：操作累計預算已耗盡。')
        if self.clock() >= self.deadline:
            raise OperationError('OPERATION_TIMEOUT：操作超過期限，請縮小範圍。')


def checkpoint(*, bytes_read=0, entries=0):
    budget = ACTIVE.get()
    if budget is not None:
        budget.bytes_read += bytes_read
        budget.entries += entries
        budget.check()


@contextmanager
def operation(budget=None):
    if ACTIVE.get() is not None:
        checkpoint()
        yield ACTIVE.get()
        return
    if not SLOTS.acquire(blocking=False):
        raise OperationError('RESOURCE_BUSY：工作名額已滿，請稍後重試。')
    budget = budget or Budget()
    token = ACTIVE.set(budget)
    try:
        budget.check()
        yield budget
    finally:
        ACTIVE.reset(token)
        SLOTS.release()


def bounded(function):
    @functools.wraps(function)
    def call(*args, **kwargs):
        with operation():
            result = function(*args, **kwargs)
            if isinstance(result, dict) and len(json.dumps(result, ensure_ascii=False, indent=2).encode('utf-8')) > 2 * 1024 * 1024:
                raise OperationError('OUTPUT_LIMIT：回傳資料超過 2 MiB，請縮小範圍。')
            return result
    return call


def asynchronous(function):
    """MCP 此版本直接執行同步方法；名額取得後才建立工作，無等待佇列。"""
    @functools.wraps(function)
    async def call(*args, **kwargs):
        if not SLOTS.acquire(blocking=False):
            raise OperationError('RESOURCE_BUSY：工作名額已滿，請稍後重試。')
        budget = Budget()
        started = threading.Event()
        def run():
            started.set()
            token = ACTIVE.set(budget)
            try:
                budget.check()
                result = function(*args, **kwargs)
                if len(json.dumps(result, ensure_ascii=False, indent=2).encode('utf-8')) > 2 * 1024 * 1024:
                    raise OperationError('OUTPUT_LIMIT：回傳資料超過 2 MiB，請縮小範圍。')
                return result
            finally:
                ACTIVE.reset(token)
                SLOTS.release()
        try:
            future = EXECUTOR.submit(run)
        except BaseException:
            SLOTS.release()
            raise
        try:
            return await asyncio.wrap_future(future)
        except BaseException:
            budget.cancel.set()
            if future.cancelled() and not started.is_set():
                SLOTS.release()
            raise
    return call


from concurrent.futures import ThreadPoolExecutor
EXECUTOR = ThreadPoolExecutor(max_workers=MAX_WORKERS, thread_name_prefix='mcp-reader')
