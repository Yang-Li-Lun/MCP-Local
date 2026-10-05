"""服務端與授權遠端驗收共用的完整輸入契約核對。"""
import json

from pathlib import Path
from mcp.server.fastmcp.utilities.func_metadata import FuncMetadata

SERVICE_VERSION = '2026.10.05.2'
CONTRACT_VERSION = 12


class StrictArguments(FuncMetadata):
    def pre_parse_json(self, data):
        """工具參數必須使用協定中的實際型別，不將字串再解碼成其他型別。"""
        return data



def contract(tools):
    return {tool.name: {'inputSchema': tool.inputSchema,
            'annotations': tool.annotations.model_dump(exclude_none=True) if tool.annotations else None} for tool in tools}


def contract_filename(access_mode='read_only'):
    from access_mode import normalize_access_mode, READ_ONLY
    mode = normalize_access_mode(access_mode)
    return 'tool-contract.json' if mode == READ_ONLY else 'tool-contract-' + mode.replace('_', '-') + '.json'


def verify_contract(tools, access_mode='read_only'):
    expected = json.loads(Path(__file__).with_name(contract_filename(access_mode)).read_text(encoding='utf-8'))
    if contract(tools) != expected:
        raise ValueError('TOOL_CONTRACT_MISMATCH：工具輸入結構或唯讀標記與本版快照不同。')
