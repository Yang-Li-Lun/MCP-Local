"""封版用真實 STDIO 驗收；僅建立及清除自身的非敏感暫存 fixture。"""
import argparse
import asyncio
import base64
import hashlib
import io
import json
import os
from pathlib import Path
import sqlite3
import sys
import tempfile
import wave
import zipfile

from PIL import Image
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

from capability_diagnostics import canonical_digest
from tool_contract import CONTRACT_VERSION, SERVICE_VERSION, contract, verify_contract


def snapshot(root: Path) -> dict:
    return {p.relative_to(root).as_posix():
            [hashlib.sha256(p.read_bytes()).hexdigest(), p.stat().st_mtime_ns]
            for p in root.rglob('*') if p.is_file()}


def fixtures(root: Path) -> None:
    (root / 'README.md').write_text('封版 UTF-8 marker\n第二行\n', encoding='utf-8')
    (root / 'pyproject.toml').write_text('[project]\nname = "release-fixture"\n', encoding='utf-8')
    (root / 'project').mkdir()
    (root / 'project' / 'pyproject.toml').write_text('[project]\nname = "nested-fixture"\n', encoding='utf-8')
    (root / 'data.bin').write_bytes(bytes(range(256)))
    (root / 'same.bin').write_bytes(bytes(range(256)))
    (root / 'state').mkdir()
    (root / 'state' / 'one.txt').write_text('one', encoding='utf-8')
    (root / 'query').mkdir()
    # The earliest creation is deliberately after the first 500 names.
    import ctypes
    from ctypes import wintypes
    import msvcrt
    setter = ctypes.WinDLL('kernel32', use_last_error=True).SetFileTime
    setter.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                      ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)]
    setter.restype = wintypes.BOOL
    for number in range(605):
        path = root / 'query' / f'f{number:04}.bin'
        path.write_bytes(bytes([number % 256]) * (number + 1))
        seconds = 946684800 if number == 604 else 1200000000 + number
        ticks = (seconds + 11644473600) * 10000000
        created = wintypes.FILETIME(ticks & 0xffffffff, ticks >> 32)
        with path.open('r+b') as handle:
            if not setter(msvcrt.get_osfhandle(handle.fileno()), ctypes.byref(created), None, None):
                raise ctypes.WinError(ctypes.get_last_error())
        os.utime(path, (1100000000 + number, 1100000000 + number))
    Image.new('RGB', (32, 24), (10, 80, 160)).save(root / 'image.png')
    with wave.open(str(root / 'audio.wav'), 'wb') as output:
        output.setnchannels(1)
        output.setsampwidth(2)
        output.setframerate(8000)
        output.writeframes(b'\0' * 16000)
    with sqlite3.connect(':memory:') as db:
        db.execute('CREATE TABLE sample(id INTEGER, label TEXT)')
        db.executemany('INSERT INTO sample VALUES(?, ?)', [(1, '第一筆'), (2, '第二筆')])
        db.commit()
        raw = db.serialize()
    (root / 'complete.sqlite').write_bytes(raw)
    # WAL header is unsupported even if no external sidecar is present.
    wal = bytearray(raw)
    wal[18:20] = b'\x02\x02'
    (root / 'wal.sqlite').write_bytes(wal)
    for name, members, compression in [
        ('safe.zip', [('hello.txt', b'archive marker')], zipfile.ZIP_DEFLATED),
        ('traversal.zip', [('../escape.txt', b'blocked')], zipfile.ZIP_STORED),
        ('bomb.zip', [('large.txt', b'0' * 1000000)], zipfile.ZIP_DEFLATED),
    ]:
        with zipfile.ZipFile(root / name, 'w', compression=compression) as archive:
            for member, data in members:
                archive.writestr(member, data)


async def verify(root: Path, report: dict) -> None:
    parameters = StdioServerParameters(command=sys.executable, args=[
        '-B', str(Path(__file__).with_name('local_files_mcp.py')), '--root', str(root)])
    async with stdio_client(parameters) as (read, write):
        async with ClientSession(read, write) as session:
            initialized = await session.initialize()
            assert initialized.serverInfo.version == SERVICE_VERSION
            tools = (await session.list_tools()).tools
            verify_contract(tools)
            assert len(tools) == 22
            report['tools'] = {tool.name: 'NOT_CALLED' for tool in tools}
            report['contract_sha256'] = canonical_digest(contract(tools))

            async def call(name: str, arguments: dict, *, reject: bool = False):
                result = await session.call_tool(name, arguments)
                assert bool(result.isError) == reject, (name, arguments, str(result.content))
                report['calls'].append(dict(tool=name, arguments=arguments,
                                            outcome='EXPECTED_REJECTION' if reject else 'PASS'))
                if reject:
                    return result
                report['tools'][name] = 'PASS'
                if name == 'read_image':
                    images = [item for item in result.content if item.type == 'image']
                    assert len(images) == 1 and images[0].mimeType == 'image/png'
                    with Image.open(io.BytesIO(base64.b64decode(images[0].data))) as image:
                        image.load()
                        assert image.size == (32, 24)
                        assert image.convert('RGB').getpixel((0, 0)) == (10, 80, 160)
                    return result
                return json.loads(result.content[0].text)

            for name, arguments, marker in [
                ('list_directory', {}, 'README.md'),
                ('list_files', {}, 'audio.wav'),
                ('read_file', {'path': 'README.md'}, '封版 UTF-8 marker'),
                ('search_text', {'query': 'marker'}, 'README.md'),
                ('list_projects', {}, 'pyproject.toml'),
                ('project_context', {}, 'README.md'),
                ('read_files', {'files': [{'path': 'README.md'}]}, '封版 UTF-8 marker'),
                ('search_texts', {'queries': ['marker', '第二行']}, 'README.md'),
                ('read_file_ranges', {'path': 'README.md', 'ranges': [{'start_line': 2, 'line_count': 1}]}, '第二行'),
                ('find_files', {'queries': ['README']}, 'README.md'),
            ]:
                result = await call(name, arguments)
                assert marker in json.dumps(result, ensure_ascii=False), (name, result)
            info = await call('workspace_info', {})
            diag = await call('server_diagnostics', {})
            assert diag['consistent'] is True
            assert diag['contract_version'] == CONTRACT_VERSION
            assert diag['actual_contract_sha256'] == diag['expected_contract_sha256'] == report['contract_sha256']
            assert info['format_limits'] == diag['format_limits']
            assert diag['format_limits']['toml_available'] and diag['format_limits']['sqlite_available']
            report['diagnostics'] = diag
            oldest = await call('query_files', dict(directory='query', sort_by='created_time', limit=1))
            assert oldest['files'][0]['path'] == 'query/f0604.bin'
            assert oldest['matched_count'] == 605 and oldest['scan_complete']
            assert 'format' not in oldest['files'][0] and 'sha256' not in oldest['files'][0]
            first = await call('query_files', dict(directory='query', sort_by='size', order='desc', limit=500))
            second_query = dict(directory='query', sort_by='size', order='desc', limit=500, cursor=first['next_cursor'])
            second_page = await call('query_files', second_query)
            combined = first['files'] + second_page['files']
            assert [r['size'] for r in combined] == list(range(605, 0, -1))
            assert len({r['path'] for r in combined}) == 605 and not second_page['has_more']
            selected = await call('query_files', dict(directory='query', name='f0604', extensions=['BIN'],
                min_size=605, max_size=605, created_before=1000000000, include_format=True,
                include_capabilities=True, include_sha256=True))
            direct = await call('file_info', dict(path='query/f0604.bin'))
            for key, value in oldest['files'][0].items():
                # NTFS may update access time after opt-in reads; compare the
                # snapshot time to the original fixture, not a later read.
                if key not in ('accessed_time', 'accessed_time_ns'):
                    assert direct[key] == value, key
            current_metadata = await call('query_files', dict(directory='query', name='f0604'))
            assert current_metadata['files'][0]['accessed_time'] == (root / 'query/f0604.bin').stat().st_atime
            assert selected['files'][0]['sha256'] == hashlib.sha256((root / 'query/f0604.bin').read_bytes()).hexdigest()
            assert selected['files'][0]['capabilities'] == direct['capabilities']
            await call('read_image', {'path': 'image.png'})
            file_info = await call('file_info', {'path': 'complete.sqlite'})
            assert file_info['format'] == 'SQLITE'
            toml = await call('read_document', {'path': 'pyproject.toml', 'format_hint': 'TOML'})
            assert toml['format'] == 'TOML' and 'release-fixture' in str(toml)
            rows = await call('read_document', {'path': 'complete.sqlite', 'table': 'sample', 'limit': 1})
            assert rows['format'] == 'SQLITE' and '第一筆' in str(rows)
            second = await call('read_document', {'path': 'complete.sqlite', 'table': 'sample',
                                'start': rows['next_start'], 'expected_sha256': rows['source_sha256']})
            assert '第二筆' in str(second)
            media = await call('inspect_media', {'path': 'audio.wav'})
            assert media['tracks'][0]['sample_rate'] == 8000
            archive = await call('inspect_archive', {'path': 'safe.zip'})
            assert 'hello.txt' in str(archive)
            member = await call('inspect_archive', {'path': 'safe.zip', 'member_path': 'hello.txt', 'length': 7})
            assert base64.b64decode(member['data']) == b'archive'
            binary = await call('read_binary', {'path': 'data.bin', 'offset': 200, 'length': 40})
            assert base64.b64decode(binary['data']) == bytes(range(200, 240))
            hashes = await call('hash_files', {'paths': ['data.bin']})
            assert hashes['files'][0]['sha256'] == hashlib.sha256(bytes(range(256))).hexdigest()
            compared = await call('compare_paths', {'left': 'data.bin', 'right': 'same.bin'})
            assert compared['counts']['same'] == 1
            baseline = await call('project_status', {'directory': 'state'})
            unchanged = await call('project_status', {'directory': 'state', 'baseline_id': baseline['baseline_id'], 'force_hash': True})
            assert unchanged['counts']['modified'] == 0 and unchanged['counts']['same'] >= 1
            before = snapshot(root)
            for name, arguments in [
                ('read_document', {'path': 'wal.sqlite'}),
                ('inspect_archive', {'path': 'traversal.zip', 'member_path': '../escape.txt'}),
                ('inspect_archive', {'path': 'bomb.zip', 'member_path': 'large.txt'}),
                ('read_document', {'path': 'complete.sqlite', 'table': 'sample; ATTACH DATABASE x'}),
                ('read_document', {'path': 'complete.sqlite', 'expected_sha256': '0' * 64}),
                ('read_binary', {'path': 'data.bin', 'length': 16385}),
                ('read_binary', {'path': 'data.bin', 'length': '10'}),
                ('read_file', {'path': '../outside.txt'}),
                ('query_files', {'directory': '../outside'}),
                ('query_files', {'limit': '1'}),
                ('query_files', {'limit': True}),
                ('query_files', {'include_sha256': 'true'}),
                ('query_files', {'sort_by': 'ctime'}),
                ('query_files', {'min_size': 10, 'max_size': 1}),
                ('query_files', {'cursor': first['next_cursor'], 'directory': 'query', 'order': 'asc'}),
                ('query_files', {'unknown': True}),
            ]:
                await call(name, arguments, reject=True)
            assert snapshot(root) == before
            assert all(value == 'PASS' for value in report['tools'].values())


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--report', type=Path, default=Path('engineering-release-stdio.json'))
    args = parser.parse_args()
    report = dict(status='FAIL', service_version=SERVICE_VERSION,
                  contract_version=CONTRACT_VERSION, python=sys.version, calls=[])
    try:
        with tempfile.TemporaryDirectory(prefix='mcp-release-') as directory:
            root = Path(directory)
            fixtures(root)
            before = snapshot(root)
            asyncio.run(verify(root, report))
            assert snapshot(root) == before, 'MCP changed fixture bytes, mtime or file inventory'
        report['fixture_unchanged'] = True
        report['status'] = 'PASS'
    except Exception as error:
        report['error'] = str(error)
        raise
    finally:
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding='utf-8')
    print(json.dumps(dict(status=report['status'], tools=len(report['tools']), calls=len(report['calls']))))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
