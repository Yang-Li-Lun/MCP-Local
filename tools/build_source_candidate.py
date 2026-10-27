"""依已審閱的固定清單建立完整 Git 來源候選；不操作 Git index 或發布。"""

if __package__ in (None, ""):
    import sys
    from pathlib import Path
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import argparse
import json
from pathlib import Path
import zipfile

from tools.build_release import MANIFEST, PAYLOAD, sha256, source_bytes, verify_package


SOURCE_LIST = 'contracts/source-delivery-files.json'


def build(output_dir: Path) -> dict:
    """封裝精確來源集合及逐檔雜湊；缺檔、連結或同名 ZIP 均拒絕。"""
    names = json.loads(source_bytes(SOURCE_LIST))['files']
    if not isinstance(names, list) or not all(isinstance(name, str) for name in names):
        raise ValueError('來源清單必須是字串陣列。')
    if names != sorted(set(names)) or SOURCE_LIST not in names:
        raise ValueError('來源清單須排序、不重複並包含自身。')
    if not set(PAYLOAD).issubset(names):
        raise ValueError('來源清單未涵蓋完整部署白名單。')
    for name in names:
        if (not name or '\\' in name or ':' in name or name.startswith('/')
                or any(part in ('', '.', '..') for part in name.split('/'))):
            raise ValueError('來源清單含不安全路徑。')
        if (name.split('/')[0] in ('archive', 'reports', 'release', 'NVIDIA Corporation')
                or any(part.startswith('.') for part in name.split('/')
                       if part not in ('.gitignore', '.gitattributes'))
                or Path(name).suffix.lower() in ('.dpapi', '.key', '.pem', '.p12', '.pfx', '.pyc', '.zip')
                or Path(name).name.startswith(('settings.json', 'local-files-gui.json'))):
            raise ValueError(f'來源清單含本機專用資料：{name}')
    files = {name: source_bytes(name) for name in names}
    manifest = {'kind': 'reviewed-working-tree-source', 'clean_clone': False,
                'files': {name: sha256(data) for name, data in files.items()}}
    output_dir = output_dir.resolve()
    output_dir.mkdir(parents=True, exist_ok=True)
    output = output_dir / 'MCP-Local-git-source-candidate.zip'
    with zipfile.ZipFile(output, 'x', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in [*files.items(), (MANIFEST, json.dumps(
                manifest, ensure_ascii=False, indent=2).encode('utf-8'))]:
            item = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            item.compress_type = zipfile.ZIP_DEFLATED
            item.create_system = 3
            item.external_attr = 0o100644 << 16
            archive.writestr(item, data)
    result = verify_package(output)
    result.update(package=output.name, kind=manifest['kind'], clean_clone=False)
    output.with_suffix('.manifest.json').write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + '\n', encoding='utf-8')
    output.with_suffix('.zip.sha256').write_text(
        f"{result['sha256']}  {output.name}\n", encoding='ascii')
    output.with_suffix('.verification.json').write_text(
        json.dumps(result, indent=2) + '\n', encoding='utf-8')
    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument('--output-dir', type=Path)
    mode.add_argument('--verify', type=Path)
    args = parser.parse_args()
    result = verify_package(args.verify) if args.verify else build(args.output_dir)
    print(json.dumps(result))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
