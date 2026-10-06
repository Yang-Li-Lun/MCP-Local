"""僅限測試的舊 AppContainer server；正式 server/CLI 不提供此入口。"""
from unittest.mock import patch

from full_control import FullControl
from local_files_mcp import create_server


def legacy_server(workspace):
    control = FullControl(workspace)
    control.sessions.toolchains = []
    with patch('developer_control.DeveloperControl', return_value=control):
        server = create_server(workspace, 'developer_control')
    workspace._access_mode = 'full_control'
    for name in ('start_process', 'interact_with_process'):
        tool = server._tool_manager.get_tool(name)
        tool.annotations = tool.annotations.model_copy(update={'destructiveHint': False})
    return server


if __name__ == '__main__':
    import sys
    from pathlib import Path
    import security_policy
    from workspace_reader import WorkspaceReader
    security_policy.STATE_DIR = Path(sys.argv[2])
    workspace = WorkspaceReader([{'id': 'main', 'path': sys.argv[1]}], 'main')
    legacy_server(workspace).run(transport='stdio')
