"""對明確指定的專用測試 Root 執行遠端唯讀驗收，不管理既有通道。"""
import argparse
import asyncio
import json
import os
from urllib.parse import urlsplit
import httpx
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client

EXPECTED = {'list_directory', 'list_files', 'read_file', 'search_text',
            'workspace_info', 'list_projects', 'project_context', 'read_files'}


async def smoke(url: str, root_id: str) -> None:
    token = os.environ.get('MCP_SMOKE_BEARER')
    headers = {'Authorization': 'Bearer ' + token} if token else {}
    async with httpx.AsyncClient(headers=headers, timeout=30) as client:
        async with streamable_http_client(url, http_client=client) as (read, write, _):
            async with ClientSession(read, write) as session:
                await session.initialize()
                tools = (await session.list_tools()).tools
                if {t.name for t in tools} != EXPECTED or not all(t.annotations and t.annotations.readOnlyHint for t in tools):
                    raise ValueError('工具清單或唯讀標記不符。')
                for name, args in [('workspace_info', {}), ('list_directory', {'root_id': root_id}),
                                   ('read_file', {'path': 'README.md', 'root_id': root_id}),
                                   ('search_text', {'query': '唯讀連線測試成功', 'root_id': root_id})]:
                    result = await session.call_tool(name, args)
                    if result.isError:
                        raise ValueError('遠端固定測試失敗。')
                    data = json.loads(result.content[0].text)
                    if name == 'read_file' and '唯讀連線測試成功' not in data.get('content', ''):
                        raise ValueError('測試文件不符。')
                    if name == 'search_text' and not data.get('matches'):
                        raise ValueError('搜尋固定字串失敗。')
                for path in ('C:\\Windows\\win.ini', '../README.md', '.hidden.txt'):
                    if not (await session.call_tool('read_file', {'path': path, 'root_id': root_id})).isError:
                        raise ValueError('遠端路徑防護測試失敗。')


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--fixture-root-id', required=True, help='僅含 shared/README.md 副本的專用 Root 代號')
    parser.add_argument('--expect-unavailable', action='store_true')
    args = parser.parse_args()
    parsed = urlsplit(args.url)
    if parsed.scheme != 'https' or parsed.username or parsed.password or parsed.query or parsed.fragment:
        parser.error('使用不含金鑰、查詢參數及認證的 HTTPS MCP 端點。')
    if args.expect_unavailable:
        token = os.environ.get('MCP_SMOKE_BEARER')
        headers = {'Authorization': 'Bearer ' + token} if token else {}
        try:
            response = httpx.get(args.url, headers=headers, timeout=15)
        except httpx.TransportError:
            print('REMOTE_TRANSPORT_UNAVAILABLE；請搭配停止前成功、重啟後恢復的結果判讀。')
            return 0
        if response.status_code in (502, 503, 504):
            print('REMOTE_GATEWAY_UNAVAILABLE；請搭配停止前成功、重啟後恢復的結果判讀。')
            return 0
        print('無法確認通道不可用；認證或一般 HTTP 錯誤不算停止驗收成功。')
        return 1
    try:
        asyncio.run(smoke(args.url, args.fixture_root_id))
    except Exception:
        # 不輸出伺服器回應或 HTTP 例外中的認證資訊。
        print('REMOTE_SMOKE_FAILED：請檢查端點、測試 Root 與認證。')
        return 1
    print('REMOTE_SMOKE_OK')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
