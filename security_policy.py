"""所有本機入口共用狀態目錄隔離；不讀取狀態內容。"""
import os
from pathlib import Path

STATE_DIR = Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local'


def validate_root_policy(root: Path) -> None:
    state = STATE_DIR.resolve()
    if root.is_relative_to(state) or (state.is_relative_to(root) and root != Path(root.anchor)):
        raise ValueError('共享範圍不可與程式設定、備份及金鑰目錄重疊。')


def validate_read_policy(path: Path) -> None:
    """整碟授權仍不得列舉或讀取本程式狀態目錄。"""
    if path.is_relative_to(STATE_DIR.resolve()):
        raise ValueError('不可讀取程式設定、備份及金鑰目錄。')
