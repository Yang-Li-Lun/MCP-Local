"""PowerShell 的共用連線入口；金鑰僅在記憶體與子程序環境中傳遞。"""
from __future__ import annotations

import getpass
import queue
import argparse
from pathlib import Path

from connection_settings import (CONFIG_DIR, CONFIG_FILE, SETTINGS_VERSION, load_settings,
                                 load_for_edit, save_settings, require_migration, normalize_connection)
from key_store import KeyStore
from connection_runtime import Connection, existing_tunnel
from power_policy import PowerPolicyManager
from access_mode import MODES, MODE_LABELS, normalize_access_mode


def set_local_mode(path: Path, mode: str) -> None:
    """明確本機 CLI 保存模式；只控制已驗證的背景連線，不終止未知 APP。"""
    from autostart_windows import get_background_status, stop_background, start_background
    mode = normalize_access_mode(mode)
    editable = load_for_edit(path)
    restart = False
    if path.resolve() == CONFIG_FILE.resolve():
        background = get_background_status()
        if background.error_code or background.running and not background.task_valid:
            raise ValueError('MODE_SWITCH_UNVERIFIED：無法確認原連線，模式未保存。')
        if background.running:
            stop_background()
            restart = True
        elif existing_tunnel():
            raise ValueError('MODE_SWITCH_APP_RUNNING：請先在原 APP 停止連線，或使用 APP 切換模式。')
    settings = {**editable.settings, 'settings_version': SETTINGS_VERSION, 'access_mode': mode}
    save_settings(settings, path, expected_revision=editable.revision if editable.revision is not None else '')
    if restart:
        start_background()


def main() -> int:
    parser = argparse.ArgumentParser(description='MCP-Local 本機連線與明確模式設定')
    parser.add_argument('--settings-file', type=Path, default=CONFIG_FILE, help='本機一般設定檔；不包含金鑰')
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument('--access-mode', choices=MODES, help='只覆寫此次啟動模式，不保存')
    modes.add_argument('--set-access-mode', choices=MODES, help='保存模式並結束；已驗證的背景連線會停止後重連')
    args = parser.parse_args()
    if args.set_access_mode:
        try:
            set_local_mode(args.settings_file, args.set_access_mode)
            print('已保存本機模式：' + MODE_LABELS[args.set_access_mode]
                  + '；手動 STDIO 連線須關閉後以新設定啟動。', flush=True)
            return 0
        except (ValueError, OSError) as error:
            print('模式未完成套用：' + str(error), flush=True)
            return 1
    power = PowerPolicyManager(args.settings_file.parent / 'power-runtime.json')
    connection = Connection(power=power)
    failed = False
    try:
        settings = load_settings(args.settings_file)
        require_migration(settings)
        if args.access_mode:
            settings = {**settings, 'settings_version': SETTINGS_VERSION, 'access_mode': args.access_mode}
        settings = normalize_connection(settings)
        power.options = settings['power']
        power.recover()
        power.apply(settings['power']['mode'])
        print(f"已授權資料夾：{len(settings['roots'])} 個", flush=True)
        print('本次模式：' + MODE_LABELS[settings['access_mode']], flush=True)
        key = KeyStore(args.settings_file.parent / 'api-key.dpapi').load()
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
