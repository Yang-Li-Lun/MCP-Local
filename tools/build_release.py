"""以固定白名單建立部署 ZIP，驗證供應商來源及解包後逐檔 SHA-256。"""

# Direct script execution and python -m use the same installed package.
if __package__ in (None, ""):
    import sys
    from pathlib import Path
    sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import argparse
import hashlib
import json
from pathlib import Path
import re
import tempfile
import zipfile


ROOT = Path(__file__).resolve().parents[1]
PAYLOAD = '''mcp_local/access_mode.py
mcp_local/control_edit.py
mcp_local/control_files.py
mcp_local/control_sessions.py
mcp_local/full_control.py
mcp_local/sandbox_windows.py
mcp_local/host_windows.py
mcp_local/host_files.py
mcp_local/host_control.py
mcp_local/developer_control.py
tools/verification/verify_access_modes_stdio.py
mcp_local/developer_vm.py
mcp_local/developer_vm_client.py
mcp_local/developer_vm_guard.py
mcp_local/developer_guest.ps1
mcp_local/developer_toolchains.py
tools/verification/verify_developer_control_stdio.py
mcp_local/autostart.py
mcp_local/autostart_windows.py
mcp_local/backup_rotation.py
mcp_local/capability_diagnostics.py
mcp_local/connection_cli.py
mcp_local/connection_runtime.py
mcp_local/connection_settings.py
mcp_local/document_extras.py
mcp_local/file_query.py
mcp_local/format_parsers.py
mcp_local/format_reader.py
mcp_local/format_worker.py
mcp_local/gui_tasks.py
mcp_local/icon_assets.py
mcp_local/image_reader.py
mcp_local/incremental_state.py
mcp_local/integrity.py
mcp_local/key_store.py
mcp_local/local_files_gui.py
mcp_local/local_files_mcp.py
mcp_local/media_parser.py
mcp_local/operation_budget.py
mcp_local/power_policy.py
mcp_local/power_restore_guard.py
mcp_local/power_windows.py
mcp_local/reader_settings.py
mcp_local/safe_logging.py
mcp_local/security_policy.py
mcp_local/snapshot_cache.py
mcp_local/stream_read.py
mcp_local/stream_search.py
mcp_local/structured_parser.py
mcp_local/tool_contract.py
mcp_local/read_call_trace.py
mcp_local/tray_windows.py
mcp_local/workspace_reader.py
mcp_local/workspace_settings.py
tools/verification/verify_release_stdio.py
requirements.txt
requirements-lock.txt
contracts/tool-contract.json
contracts/tool-contract-full-control.json
contracts/tool-contract-developer-control.json
contracts/tool-contract-host-control.json
third_party/tunnel-client/vendor-integrity.json
third_party/tunnel-client/LICENSE
third_party/tunnel-client/NOTICE
LICENSE
NOTICE
assets/icons/mcp-local.ico
start-local-files-tunnel.ps1
開啟連線設定.vbs
docs/INDEX.md
README.md
README_繁體中文.md
RELEASE.md
docs/guides/介面使用說明.md
docs/guides/自啟動與優化說明.md
docs/history/低功耗模式工程設計.md
docs/history/性能優化交付報告.md
docs/history/體感效能優化交付報告.md
docs/guides/格式讀取說明.md
docs/guides/圖片讀取說明.md
docs/guides/增量與診斷工具說明.md
docs/architecture/完整控制模式.md
third_party/DesktopCommanderMCP/LICENSE
third_party/DesktopCommanderMCP/NOTICE
assets/icons/mcp-local-classic.png
assets/icons/mcp-local-v2.png
assets/icons/mcp-local-v2.ico
shared/README.md
shared/image-acceptance.png
mcp_local/__init__.py
access_mode.py
tools/__init__.py
tools/verification/__init__.py
local_files_mcp.py
local_files_gui.py
connection_cli.py
autostart.py
docs/PROJECT_LAYOUT.md'''.split()
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
    expected = json.loads(source_bytes('third_party/tunnel-client/vendor-integrity.json'))['tunnel-client.exe']
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
        if files[name] != source_bytes('third_party/tunnel-client/' + name):
            raise ValueError(f'供應商檔案與既有版本不符：{name}')
    return {'third_party/tunnel-client/' + name: data for name, data in files.items()}


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
    parser.add_argument('--output-dir', type=Path, default=ROOT / 'release',
                        help='新封裝輸出目錄；拒絕覆寫既有 ZIP')
    args = parser.parse_args()
    if args.verify:
        print(json.dumps(verify_package(args.verify)))
        return 0
    files = {name: source_bytes(name) for name in PAYLOAD}
    if args.vendor_archive:
        files.update(vendor_bytes(args.vendor_archive))
    version = re.search(r"SERVICE_VERSION = '([^']+)'", files['mcp_local/tool_contract.py'].decode())[1]
    flavor = 'windows-amd64' if args.vendor_archive else 'windows-source'
    output = args.output_dir.resolve() / f'MCP-Local-{version}-{flavor}.zip'
    output.parent.mkdir(parents=True, exist_ok=True)
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
