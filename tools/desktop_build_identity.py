"""桌面建置來源清單；只讀取固定程式來源，不讀設定或憑證。"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
from datetime import datetime, timezone

ROOT = Path(__file__).resolve().parents[1]


def digest(files: dict[str, str]) -> str:
    return hashlib.sha256(json.dumps(files, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def source_files(root: Path) -> dict[str, str]:
    paths = list((root / 'mcp_local').glob('*.py'))
    paths += [root / name for name in ('local_files_mcp.py', 'access_mode.py', 'connection_cli.py', 'autostart.py', 'local_files_gui.py')]
    return {p.relative_to(root).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest() for p in sorted(paths)}


def source_commit(root: Path) -> dict:
    try:
        options = dict(cwd=root, capture_output=True, text=True, timeout=3,
                       creationflags=getattr(subprocess, 'CREATE_NO_WINDOW', 0))
        head = subprocess.run(['git', 'rev-parse', 'HEAD'], **options)
        status = subprocess.run(['git', 'status', '--porcelain', '--untracked-files=no'], **options)
        if head.returncode or status.returncode:
            raise ValueError('git unavailable')
        return {'commit': head.stdout.strip(), 'tracked_dirty': bool(status.stdout.strip()),
                'scope': 'Git HEAD 為來源基底；精確建置內容以來源雜湊辨識。'}
    except (OSError, ValueError, subprocess.SubprocessError):
        return {'commit': '未知', 'tracked_dirty': '未知'}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--artifacts', type=Path)
    args = parser.parse_args()
    if args.artifacts:
        manifest = json.loads(args.output.read_text(encoding='utf-8'))
        manifest['artifacts'] = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()
                                 for p in sorted(args.artifacts.glob('McpLocal.Desktop.*'))
                                 if p.suffix in ('.exe', '.dll', '.json', '.pdb')}
    else:
        backend = source_files(ROOT)
        frontend = {p.relative_to(ROOT).as_posix(): hashlib.sha256(p.read_bytes()).hexdigest()
                    for p in sorted((ROOT / 'desktop/wpf').iterdir())
                    if p.is_file() and p.suffix in ('.cs', '.xaml', '.csproj', '.Config')}
        manifest = {'schema': 1, 'built_at_utc': datetime.now(timezone.utc).isoformat(),
                    'source': source_commit(ROOT), 'frontend_build_id': digest(frontend),
                    'backend_build_id': digest(backend), 'frontend_sources': frontend, 'backend_sources': backend}
        manifest['deployment_id'] = digest({**frontend, **backend})
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(manifest, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')


if __name__ == '__main__':
    main()
