"""相容啟動入口；實作位於 tools.build_release。"""
import sys
from tools import build_release as _implementation

if __name__ == '__main__':
    raise SystemExit(_implementation.main())
else:
    sys.modules[__name__] = _implementation
