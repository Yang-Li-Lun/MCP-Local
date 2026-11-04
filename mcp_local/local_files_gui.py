"""正式桌面相容入口；唯一介面為 WPF，設定與連線由 Python 後端管理。"""
from mcp_local.desktop_launcher import main


if __name__ == '__main__':
    raise SystemExit(main())
