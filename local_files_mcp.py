"""相容啟動入口；實作位於 mcp_local.local_files_mcp。"""
import sys
from mcp_local import local_files_mcp as _implementation

if __name__ == '__main__':
    try:
        _implementation.main()
    except KeyboardInterrupt:
        pass
    except Exception:
        print('啟動失敗：請檢查共享資料夾、設定格式與必要套件。', file=sys.stderr)
        sys.exit(1)
else:
    sys.modules[__name__] = _implementation
