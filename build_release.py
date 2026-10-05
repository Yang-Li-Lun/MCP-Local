"""以固定白名單建立部署 ZIP，驗證供應商來源及解包後逐檔 SHA-256。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parent
PAYLOAD = '''access_mode.py control_edit.py control_files.py control_sessions.py
full_control.py sandbox_windows.py verify_full_control_stdio.py
host_windows.py host_files.py host_control.py developer_control.py verify_access_modes_stdio.py
developer_vm.py developer_vm_client.py developer_vm_guard.py developer_guest.ps1 verify_developer_control_stdio.py
autostart.py autostart_windows.py backup_rotation.py
capability_diagnostics.py connection_cli.py connection_runtime.py connection_settings.py
document_extras.py file_query.py format_parsers.py format_reader.py format_worker.py gui_tasks.py
icon_assets.py image_reader.py incremental_state.py integrity.py key_store.py
local_files_gui.py local_files_mcp.py media_parser.py operation_budget.py power_policy.py
power_restore_guard.py power_windows.py reader_settings.py safe_logging.py security_policy.py
snapshot_cache.py stream_read.py stream_search.py structured_parser.py tool_contract.py
tray_windows.py workspace_reader.py workspace_settings.py verify_release_stdio.py
requirements.txt requirements-lock.txt tool-contract.json tool-contract-full-control.json
tool-contract-developer-control.json tool-contract-host-control.json vendor-integrity.json
LICENSE NOTICE mcp-local.ico start-local-files-tunnel.ps1 開啟連線設定.vbs
README.md README_繁體中文.md RELEASE.md 介面使用說明.md 自啟動與優化說明.md
低功耗模式工程設計.md 性能優化交付報告.md 體感效能優化交付報告.md
格式讀取說明.md 圖片讀取說明.md 增量與診斷工具說明.md 完整控制模式.md
third_party/DesktopCommanderMCP/LICENSE third_party/DesktopCommanderMCP/NOTICE
assets/icons/mcp-local-classic.png assets/icons/mcp-local-v2.png assets/icons/mcp-local-v2.ico
shared/README.md shared/image-acceptance.png'''.split()
MANIFEST = 'SHA256SUMS.json'


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def source_bytes(name: str) -> bytes:
    path = ROOT / name
    for node in [path, *path.parents]:
        if node == ROOT:
            break
        info = node.lstat()
        if node.is_symlink() or getattr(info, 'st_file_attributes', 0) & 0x400:
            raise ValueError(f'拒絕連結或 reparse point：{name}')
    if path.stat().st_nlink != 1:
        raise ValueError(f'拒絕 hard link：{name}')
    return path.read_bytes()


def vendor_bytes(archive_path: Path) -> dict:
    expected = json.loads(source_bytes('vendor-integrity.json'))['tunnel-client.exe']
    version = expected['version']
    prefix = f'tunnel-client-{version}-windows-amd64'
    names = {'tunnel-client.exe', 'cloudflared.exe', 'cloudflared-manifest.json',
             'LICENSE', 'NOTICE', f'{prefix}-licenses.txt', f'{prefix}.spdx.json'}
    with zipfile.ZipFile(archive_path) as archive:
        if set(archive.namelist()) != names or len(archive.infolist()) != len(names):
            raise ValueError('供應商封裝檔案清單不符。')
        if any(item.file_size > 128 * 1024 * 1024 for item in archive.infolist()):
            raise ValueError('供應商成品超過封裝大小上限。')
        files = {name: archive.read(name) for name in names}
    if sha256(files['tunnel-client.exe']) != expected['sha256']:
        raise ValueError('供應商 tunnel-client 與固定 integrity 不符。')
    spdx = json.loads(files[f'{prefix}.spdx.json'])
    checked = set()
    for item in spdx['files']:
        name = item['fileName']
        if name not in files:
            raise ValueError('SPDX 包含未知檔案。')
        digest = next(c['checksumValue'] for c in item['checksums'] if c['algorithm'] == 'SHA256')
        if sha256(files[name]) != digest:
            raise ValueError(f'SPDX 完整性不符：{name}')
        checked.add(name)
    if checked != names - {f'{prefix}.spdx.json'}:
        raise ValueError('SPDX 未涵蓋全部供應商成品。')
    for name in ('LICENSE', 'NOTICE', 'cloudflared-manifest.json'):
        if files[name] != source_bytes(name):
            raise ValueError(f'供應商檔案與既有版本不符：{name}')
    return files


def verify_package(package: Path) -> dict:
    """驗證精確清單、路徑及解包後內容；不執行任何封包內程式。"""
    with zipfile.ZipFile(package) as archive:
        names = archive.namelist()
        manifest = json.loads(archive.read(MANIFEST))
        expected = manifest['files']
        if len(names) != len(set(names)) or set(names) != set(expected) | {MANIFEST}:
            raise ValueError('封包檔案清單不符。')
        with tempfile.TemporaryDirectory(prefix='mcp-unpack-') as directory:
            destination = Path(directory).resolve()
            for name, digest in expected.items():
                path = destination / name
                if (not name or '\\' in name or ':' in name or
                        any(part in ('', '.', '..') for part in name.split('/')) or
                        name.startswith('/') or not path.resolve().is_relative_to(destination)):
                    raise ValueError('封包包含不安全路徑。')
                data = archive.read(name)
                if sha256(data) != digest:
                    raise ValueError(f'封包 SHA-256 不符：{name}')
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                if sha256(path.read_bytes()) != digest:
                    raise ValueError(f'解包 SHA-256 不符：{name}')
    return dict(files=len(expected), sha256=sha256(package.read_bytes()), unpack_verified=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--vendor-archive', type=Path,
                        help='選用既有官方 ZIP；驗證固定 tunnel digest 及全部 SPDX 雜湊，不下載、不啟動')
    parser.add_argument('--verify', type=Path, help='只驗證既有封包及解包後 SHA-256')
    args = parser.parse_args()
    if args.verify:
        print(json.dumps(verify_package(args.verify)))
        return 0
    files = {name: source_bytes(name) for name in PAYLOAD}
    if args.vendor_archive:
        files.update(vendor_bytes(args.vendor_archive))
    version = re.search(r"SERVICE_VERSION = '([^']+)'", files['tool_contract.py'].decode())[1]
    flavor = 'windows-amd64' if args.vendor_archive else 'windows-source'
    output = ROOT / 'release' / f'MCP-Local-{version}-{flavor}.zip'
    output.parent.mkdir(exist_ok=True)
    manifest = dict(service_version=version, vendor_included=bool(args.vendor_archive),
                    files={name: sha256(data) for name, data in sorted(files.items())})
    # 固定 ZIP timestamp，避免同一份來源因打包時間產生不同雜湊。
    with zipfile.ZipFile(output, 'x', compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for name, data in sorted(files.items()):
            item = zipfile.ZipInfo(name, date_time=(2026, 1, 1, 0, 0, 0))
            item.compress_type = zipfile.ZIP_DEFLATED
            archive.writestr(item, data)
        item = zipfile.ZipInfo(MANIFEST, date_time=(2026, 1, 1, 0, 0, 0))
        item.compress_type = zipfile.ZIP_DEFLATED
        archive.writestr(item, json.dumps(manifest, ensure_ascii=False, indent=2).encode('utf-8'))
    result = verify_package(output)
    result.update(package=output.name, service_version=version,
                  vendor_included=bool(args.vendor_archive))
    output.with_suffix('.zip.sha256').write_text(f"{result['sha256']}  {output.name}\n", encoding='ascii')
    output.with_suffix('.manifest.json').write_text(json.dumps(manifest, ensure_ascii=False, indent=2), encoding='utf-8')
    output.with_suffix('.verification.json').write_text(json.dumps(result, indent=2), encoding='utf-8')
    print(json.dumps(result))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
