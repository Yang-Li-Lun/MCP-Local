"""相容匯入入口；三模式定義位於 mcp_local.access_mode。"""
import sys
from mcp_local import access_mode as _implementation

sys.modules[__name__] = _implementation
