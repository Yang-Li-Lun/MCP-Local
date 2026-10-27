"""Select flat historical sources or the current package without mixing revisions."""
import importlib
from pathlib import Path
import sys
from types import ModuleType


def source_module(source: Path, name: str) -> ModuleType:
    root = source.resolve(strict=True)
    sys.path.insert(0, str(root))
    qualified = 'mcp_local.' + name if (root / 'mcp_local/__init__.py').is_file() else name
    module = importlib.import_module(qualified)
    if not Path(module.__file__).resolve().is_relative_to(root):
        raise ValueError('Benchmark source already loaded from a different directory; use a fresh process')
    return module
