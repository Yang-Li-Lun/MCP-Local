"""相容啟動入口；實作位於 mcp_local.local_files_gui。"""
import sys
from mcp_local import local_files_gui as _implementation

if __name__ == '__main__':
    _implementation.main()
else:
    sys.modules[__name__] = _implementation
