"""不建立 Tk 視窗的登入連線入口。"""
import ctypes
import queue
import time
from connection_settings import CONFIG_DIR, load_settings, require_migration, build_commands
from connection_runtime import Connection, ConnectionErrorKind, RetryPolicy
from key_store import KeyStore
from safe_logging import create_logger


def main() -> int:
    from tray_windows import kernel
    mutex = kernel.CreateMutexW(None, False, 'Local\\MCP_Local_Background')
    if not mutex:
        return 1
    duplicate = ctypes.get_last_error() == 183
    connection = Connection()
    try:
        if duplicate:
            return 0
        logger = create_logger(CONFIG_DIR)
        policy = RetryPolicy()
        while True:
            kind = ConnectionErrorKind.CONFIG_INVALID
            try:
                settings = load_settings()
                if not settings['auto_start'] or not settings['auto_connect']:
                    return 0
                kind = ConnectionErrorKind.MIGRATION_REQUIRED
                require_migration(settings)
                kind = ConnectionErrorKind.CLIENT_INTEGRITY_FAILED
                build_commands(settings)
                kind = ConnectionErrorKind.KEY_DECRYPT_FAILED
                key = KeyStore(CONFIG_DIR / 'api-key.dpapi').load()
                if not key:
                    kind = ConnectionErrorKind.KEY_MISSING
                    raise ValueError()
            except Exception:
                logger.info(kind.value)
                return 0  # 永久失敗不觸發排程器的失敗重啟。
            connection.start(settings, key)
            key = ''
            while True:
                try:
                    event, _ = connection.events.get(timeout=1)
                except queue.Empty:
                    continue
                if event == 'status':
                    logger.info('RUNNING' if connection.running_since else 'STARTING')
                if event == 'done':
                    break
            connection.thread.join(timeout=10)
            kind = connection.error_kind
            logger.info(kind.value if kind else 'STOPPED')
            if kind not in policy.retryable:
                return 0
            # 只有持續運作至少一分鐘才重置，避免立即退出迴圈高速重試。
            if connection.running_since and time.monotonic() - connection.running_since >= 60:
                policy.reset()
            time.sleep(policy.next_delay())
    finally:
        connection.stop()
        if connection.thread:
            connection.thread.join(timeout=10)
        kernel.CloseHandle(mutex)


if __name__ == '__main__':
    raise SystemExit(main())
