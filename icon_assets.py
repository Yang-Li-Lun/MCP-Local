"""內建圖示清單；設定只能選取固定資產，不接受任意路徑。"""
from pathlib import Path

PROJECT = Path(__file__).resolve().parent
ICON_STYLES = {'classic': '原版圖示', 'folder_link': '資料夾連結'}


def normalize_icon_style(value: str = 'classic') -> str:
    if not isinstance(value, str) or value not in ICON_STYLES:
        raise ValueError('圖示樣式必須為內建的原版圖示或資料夾連結。')
    return value


def icon_path(style: str = 'classic', *, preview: bool = False) -> Path:
    style = normalize_icon_style(style)
    if style == 'classic':
        return PROJECT / ('assets/icons/mcp-local-classic.png' if preview else 'mcp-local.ico')
    return PROJECT / ('assets/icons/mcp-local-v2.png' if preview else 'assets/icons/mcp-local-v2.ico')
