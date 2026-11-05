"""私有桌面啟動識別；記錄實際編譯執行的本專案 Python 位元組。"""
from __future__ import annotations

import hashlib
import importlib.abc
import importlib.machinery
import json
import os
from pathlib import Path
import sys
import uuid
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]
STARTED = datetime.now(timezone.utc).isoformat()
START_ID = uuid.uuid4().hex
LOADED: dict[str, str] = {}
MANIFEST: dict = {}
DISK_SOURCE: dict = {'commit': '未知', 'tracked_dirty': '未知'}


class SourceLoader(importlib.machinery.SourceFileLoader):
    def get_code(self, fullname):
        # Compile exactly the bytes whose hash is recorded. Do not infer loaded code
        # from a later disk read or an mtime-valid .pyc.
        data = self.get_data(self.path)
        code = self.source_to_code(data, self.path)
        LOADED[Path(self.path).relative_to(ROOT).as_posix()] = hashlib.sha256(data).hexdigest()
        return code


class SourceFinder(importlib.abc.MetaPathFinder):
    def find_spec(self, fullname, path=None, target=None):
        spec = importlib.machinery.PathFinder.find_spec(fullname, path, target)
        if spec and isinstance(spec.loader, importlib.machinery.SourceFileLoader) and spec.origin:
            origin = Path(spec.origin)
            if origin.parent == ROOT / 'mcp_local' or (origin.parent == ROOT and origin.name in
                    ('local_files_mcp.py', 'access_mode.py', 'connection_cli.py', 'autostart.py', 'local_files_gui.py')):
                spec.loader = SourceLoader(fullname, spec.origin)
        return spec


def diagnostics() -> dict:
    expected = MANIFEST.get('backend_sources', {})
    matching = bool(LOADED and expected) and all(expected.get(name) == value for name, value in LOADED.items())
    disk = {}
    for name in expected:
        # Manifest paths are descriptive data, never arbitrary read authorization.
        relative = Path(name)
        if relative.is_absolute() or '..' in relative.parts or relative.suffix != '.py':
            continue
        if relative.parts[0] not in ('mcp_local', 'local_files_mcp.py', 'access_mode.py', 'connection_cli.py', 'autostart.py', 'local_files_gui.py'):
            continue
        try:
            disk[name] = hashlib.sha256((ROOT / relative).read_bytes()).hexdigest()
        except OSError:
            disk[name] = '未知'
    canonical = json.dumps(LOADED, sort_keys=True, separators=(',', ':')).encode()
    return {'process_id': os.getpid(), 'process_start_id': START_ID, 'started_at_utc': STARTED,
            'python': sys.version.split()[0], 'executable': sys.executable,
            'loaded_code_sha256': hashlib.sha256(canonical).hexdigest() if LOADED else '未知',
            'loaded_modules': dict(LOADED),
            'loaded_scope': '本程序經來源 loader 編譯的專案模組；不含啟動識別模組、套件初始化及第三方依賴；未載入模組未知。',
            'backend_build_id': MANIFEST.get('backend_build_id', '未知') if matching else '未知',
            'build_source': MANIFEST.get('source', '未知') if matching else '未知',
            'loaded_matches_build': matching if expected and LOADED else '未知',
            'disk_source_at_start': DISK_SOURCE,
            'disk_matches_build_now': disk == expected if expected else '未知',
            'disk_matches_loaded_now': all(disk.get(name) == value for name, value in LOADED.items()) if disk and LOADED else '未知',
            'deployment_id': MANIFEST.get('deployment_id', '未知'),
            'deployment_artifacts': MANIFEST.get('artifacts', '未知')}


def main() -> int:
    global MANIFEST, DISK_SOURCE
    # This bootstrap owns the module identity even when started with python -m.
    sys.modules['mcp_local.desktop_identity'] = sys.modules[__name__]
    if len(sys.argv) >= 3 and sys.argv[1] == '--build-manifest':
        manifest = Path(sys.argv[2])
        del sys.argv[1:3]
        try:
            if manifest.stat().st_size <= 1024 * 1024:
                value = json.loads(manifest.read_text(encoding='utf-8'))
                if value.get('schema') == 1:
                    MANIFEST = value
        except (OSError, ValueError, AttributeError):
            pass
    # Git here is read-only and precedes fixture's deliberately stricter child-process guard.
    try:
        from tools.desktop_build_identity import source_commit
        DISK_SOURCE = source_commit(ROOT)
    except ImportError:
        pass
    sys.meta_path.insert(0, SourceFinder())
    from mcp_local.desktop_bridge import main as bridge_main
    return bridge_main()


if __name__ == '__main__':
    raise SystemExit(main())
