"""經明確授權後，用 Responses API 驗證唯一的非敏感固定文件。"""
import argparse
import json
import httpx
from connection_settings import CONFIG_DIR
from key_store import KeyStore

FIXTURE = {'path': 'MCP-Local/shared/README.md', 'root_id': 'main',
           'start_line': 1, 'line_count': 10}


def validate_approval(item: dict) -> bool:
    try:
        return (item.get('type') == 'mcp_approval_request' and item.get('name') == 'read_file'
                and item.get('server_label') == 'local_fixture'
                and json.loads(item.get('arguments', '{}')) == FIXTURE)
    except (ValueError, TypeError):
        return False


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--model', required=True, help='明確指定可用的 Responses API 模型；會產生 API 用量')
    args = parser.parse_args()
    try:
        settings = json.loads((CONFIG_DIR / 'settings.json').read_text(encoding='utf-8'))
        key = KeyStore(CONFIG_DIR / 'api-key.dpapi').load()
        if not key:
            raise ValueError()
        tools = [{'type': 'mcp', 'server_label': 'local_fixture', 'tunnel_id': settings['tunnel'],
                  'allowed_tools': ['read_file'], 'require_approval': 'always'}]
        with httpx.Client(headers={'Authorization': 'Bearer ' + key}, timeout=45) as client:
            response = client.post('https://api.openai.com/v1/responses', json={
                'model': args.model, 'max_output_tokens': 512,
                'input': 'Call read_file exactly once with these exact arguments: ' + json.dumps(FIXTURE) +
                         '. Do not read any other file.',
                'tools': tools, 'tool_choice': {'type': 'mcp', 'server_label': 'local_fixture', 'name': 'read_file'}})
            if response.status_code != 200:
                print('RESPONSES_HTTP_' + str(response.status_code))
                return 1
            data = response.json()
            approvals = [item for item in data.get('output', []) if item.get('type') == 'mcp_approval_request']
            if len(approvals) != 1 or not validate_approval(approvals[0]):
                print('EXPECTED_FIXTURE_APPROVAL_MISSING；未核准任何其他工具參數。')
                return 1
            response = client.post('https://api.openai.com/v1/responses', json={
                'model': args.model, 'max_output_tokens': 512, 'previous_response_id': data['id'],
                'tools': tools, 'tool_choice': 'none',
                'input': [{'type': 'mcp_approval_response', 'approval_request_id': approvals[0]['id'], 'approve': True}]})
            if response.status_code != 200:
                print('FIXTURE_HTTP_' + str(response.status_code))
                return 1
            for item in response.json().get('output', []):
                if item.get('type') == 'mcp_call' and item.get('name') == 'read_file' and not item.get('error'):
                    output = item.get('output', '')
                    try:
                        output = json.dumps(json.loads(output), ensure_ascii=False)
                    except (ValueError, TypeError):
                        pass
                    if '唯讀連線測試成功' in output:
                        print('SECURE_TUNNEL_FIXTURE_OK')
                        return 0
            print('REMOTE_FIXTURE_NOT_VERIFIED')
            return 1
    except Exception:
        print('REMOTE_TEST_FAILED；未輸出金鑰、伺服器回應或原始例外。')
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
