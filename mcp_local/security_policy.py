"""共用狀態目錄隔離與主機 Python 寫入保護；不讀取設定內容。"""
import os
from pathlib import Path
import site
import sys

STATE_DIR = Path(os.environ.get('LOCALAPPDATA', str(Path.home()))) / 'MCP-Local'


def python_runtime_paths() -> tuple[Path, ...]:
    """涵蓋執行中的安裝、venv、執行檔與匯入路徑，包含尚不存在的 ZIP。"""
    # Base-interpreter children can enable user site even when this venv disables it.
    values = (sys.prefix, sys.exec_prefix, sys.base_prefix, sys.base_exec_prefix,
              site.getusersitepackages(), *sys.path)
    # Empty/relative import entries are relative to the process working directory.
    paths = [Path(value) for value in values if isinstance(value, str)]
    paths.extend(Path(value).parent for value in (sys.executable, getattr(sys, '_base_executable', None))
                 if isinstance(value, str) and value)
    return tuple(dict.fromkeys(path.resolve() for path in paths))


def overlaps_python_runtime(path: Path) -> bool:
    """禁止修改執行環境及其祖先；只供 mutation 與可寫 VM 映射使用。"""
    candidate = path.resolve()
    return any(candidate.is_relative_to(root) or root.is_relative_to(candidate)
               for root in python_runtime_paths())


def validate_root_policy(root: Path) -> None:
    state = STATE_DIR.resolve()
    if root.is_relative_to(state) or (state.is_relative_to(root) and root != Path(root.anchor)):
        raise ValueError('共享範圍不可與程式設定、備份及金鑰目錄重疊。')


def validate_read_policy(path: Path) -> None:
    """整碟授權仍不得列舉或讀取本程式狀態目錄。"""
    # 每次重新解析狀態目錄；逐段比較避免 is_relative_to 建立所有祖先 Path。
    state = STATE_DIR.resolve()
    parts, protected = path.parts, state.parts
    if len(parts) >= len(protected) and all(
            os.path.normcase(left) == os.path.normcase(right)
            for left, right in zip(parts, protected)):
        raise ValueError('不可讀取程式設定、備份及金鑰目錄。')
