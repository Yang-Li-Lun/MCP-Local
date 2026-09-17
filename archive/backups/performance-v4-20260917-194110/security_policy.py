"""所有本機入口共用狀態目錄隔離；不讀取狀態內容。"""
import os
from pathlib import Path

STATE_DIR = Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local'


def validate_root_policy(root: Path) -> None:
    state = STATE_DIR.resolve()
    if root.is_relative_to(state) or state.is_relative_to(root):
        raise ValueError('共享範圍不可與程式設定、備份及金鑰目錄重疊。')
