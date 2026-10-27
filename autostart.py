"""相容啟動入口；實作位於 mcp_local.autostart。"""
import sys
from mcp_local import autostart as _implementation

if __name__ == '__main__':
    raise SystemExit(_implementation.main())
else:
    sys.modules[__name__] = _implementation
