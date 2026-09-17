"""PowerShell 的共用連線入口；金鑰僅在記憶體與子程序環境中傳遞。"""
from __future__ import annotations

import getpass
import queue

from connection_settings import CONFIG_DIR, load_settings, require_migration, normalize_connection
from key_store import KeyStore
from connection_runtime import Connection
from power_policy import PowerPolicyManager


def main() -> int:
    power = PowerPolicyManager(CONFIG_DIR / 'power-runtime.json')
    connection = Connection(power=power)
    failed = False
    try:
        settings = load_settings()
        require_migration(settings)
        settings = normalize_connection(settings)
        power.options = settings['power']
        power.recover()
        power.apply(settings['power']['mode'])
        print(f"已授權資料夾：{len(settings['roots'])} 個", flush=True)
        key = KeyStore(CONFIG_DIR / 'api-key.dpapi').load()
        if not key:
            key = getpass.getpass('請輸入通道 API 金鑰（隱藏輸入）：').strip()
        if not key:
            raise ValueError('未提供金鑰。')
        connection.start(settings, key)
        key = ''
        while True:
            try:
                kind, message = connection.events.get(timeout=.2)
            except queue.Empty:
                continue
            print(message + ('；遠端可用性請以實際工具呼叫確認。' if message == '通道程序執行中' else ''), flush=True)
            failed |= kind == 'error'
            if kind == 'done':
                return int(failed)
    except KeyboardInterrupt:
        return 0
    except Exception:
        # 設定驗證的具體訊息可安全顯示；金鑰保存層錯誤不輸出原始資料。
        print('無法啟動：請從連線設定介面檢查設定遷移、分享範圍及加密金鑰。', flush=True)
        return 1
    finally:
        connection.stop()
        if connection.thread:
            connection.thread.join(timeout=10)
        power.close()


if __name__ == '__main__':
    raise SystemExit(main())
