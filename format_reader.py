"""Generic read-only access; only validated byte snapshots reach format parsers."""
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import hashlib
import sqlite3
from typing import Any, BinaryIO

from operation_budget import checkpoint

MAX_SOURCE = 32 * 1024 * 1024
HEADER_BYTES = 4096
MAX_RANGE = 16 * 1024
PARSER_TIMEOUT = 10
PARSER_MEMORY = 512 * 1024 * 1024
MAX_RESPONSE = 256 * 1024


def format_limits() -> dict:
    return dict(max_source_bytes=MAX_SOURCE, source_policy='min(root.max_file_bytes, root.max_scan_bytes, max_source_bytes)',
                max_path_chars=4096, max_root_id_chars=32,
                header_bytes=HEADER_BYTES, max_range_bytes=MAX_RANGE,
                parser_timeout_seconds=PARSER_TIMEOUT, parser_cpu_seconds=10, parser_memory_bytes=PARSER_MEMORY,
                max_response_bytes=MAX_RESPONSE, max_units_per_call=100, max_unit_chars=2000,
                max_document_page_bytes=128*1024, max_start=100000,
                max_document_units=100000, max_pdf_pages=2000, max_pdf_stream_bytes=8*1024*1024,
                max_pdf_page_tree_nodes=4000, max_pdf_page_tree_depth=64, max_pdf_xform_invocations=1000,
                max_archive_entries=4096, max_archive_member_bytes=8*1024*1024,
                max_archive_expanded_bytes=128*1024*1024, max_compression_ratio=200,
                max_zip_directory_bytes=4*1024*1024, max_gzip_expanded_bytes=32*1024*1024,
                max_archive_name_chars=1024,
                max_xml_nodes=200000, max_xml_depth=64, archive_member_read=True,
                structured_formats=['CSV', 'TSV', 'JSON', 'JSONL', 'XML', 'YAML', 'INI', 'TOML (Python 3.11+)', 'SQLITE (deserialize/setlimit required)'],
                document_formats=['PDF', 'DOCX', 'XLSX', 'PPTX', 'ODT', 'ODS', 'ODP', 'EPUB', 'EML', 'MBOX', 'SRT', 'VTT'],
                max_structured_nodes=100000, max_structured_depth=64, max_columns=256,
                max_csv_field_chars=128*1024, max_sqlite_value_bytes=1024*1024,
                max_sqlite_tables=1024, max_sqlite_vm_steps=20000000,
                max_mail_messages=1000, max_mime_parts=1000, max_mime_depth=32,
                media_formats=['MP3', 'FLAC', 'OGG', 'WAV', 'MP4', 'AVI', 'AIFF', 'AU', 'MATROSKA', 'WEBM', 'static image metadata (see image_limits)'],
                source_sha256=True, continuation='expected_sha256 optionally pins the source snapshot',
                toml_available=sys.version_info >= (3, 11),
                sqlite_available=hasattr(sqlite3.Connection, 'deserialize') and hasattr(sqlite3.Connection, 'setlimit'),
                yaml_loader='PyYAML 6.0.3 pure Python SafeLoader; no anchors, aliases, explicit tags or merge',
                max_media_tracks=32, max_media_boxes=10000, max_media_tags=32,
                max_ebml_elements=10000, ebml_unknown_sizes='Segment only; unknown-size Cluster rejected',
                max_media_tag_key_chars=128, max_media_tag_values=4, max_media_tag_value_chars=1024,
                max_mp4_timing_entries=100000, max_avi_list_depth=8,
                metadata_for_oversized_files=True, parser_network=False, parser_source_paths=False)


def sniff(data: bytes) -> tuple[str, str]:
    signatures = [(b'%PDF-', 'PDF', 'application/pdf'), (b'\x89PNG\r\n\x1a\n', 'PNG', 'image/png'),
                  (b'\xff\xd8\xff', 'JPEG', 'image/jpeg'), (b'GIF87a', 'GIF', 'image/gif'),
                  (b'GIF89a', 'GIF', 'image/gif'), (b'BM', 'BMP', 'image/bmp'),
                  (b'II*\x00', 'TIFF', 'image/tiff'), (b'MM\x00*', 'TIFF', 'image/tiff'),
                  (b'PK\x03\x04', 'ZIP', 'application/zip'), (b'PK\x05\x06', 'ZIP', 'application/zip'),
                  (b'\x1f\x8b', 'GZIP', 'application/gzip'), (b'7z\xbc\xaf\x27\x1c', '7Z', 'application/x-7z-compressed'),
                  (b'Rar!\x1a\x07', 'RAR', 'application/vnd.rar'), (b'\xfd7zXZ\x00', 'XZ', 'application/x-xz'),
                  (b'BZh', 'BZIP2', 'application/x-bzip2'), (b'fLaC', 'FLAC', 'audio/flac'),
                  (b'OggS', 'OGG', 'application/ogg'), (b'ID3', 'MP3', 'audio/mpeg'),
                  (b'\x1aE\xdf\xa3', 'MATROSKA', 'video/x-matroska'),
                  (b'\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1', 'OLE', 'application/x-ole-storage'),
                  (b'MZ', 'PE/DOS', 'application/vnd.microsoft.portable-executable'),
                  (b'\x7fELF', 'ELF', 'application/x-elf'), (b'SQLite format 3\x00', 'SQLITE', 'application/vnd.sqlite3')]
    for signature, name, mime in signatures:
        if data.startswith(signature):
            return name, mime
    if len(data) >= 12 and data[:4] == b'RIFF':
        return {b'WEBP': ('WEBP', 'image/webp'), b'WAVE': ('WAV', 'audio/wav'),
                                b'AVI ': ('AVI', 'video/x-msvideo')}.get(data[8:12], ('RIFF', 'application/octet-stream'))
    if data.startswith(b'\x00\x00\x01\x00'):
        return 'ICO', 'image/x-icon'
    if len(data) >= 3 and data[:2] in (b'P1', b'P2', b'P3', b'P4', b'P5', b'P6') and data[2:3] in b' \t\r\n':
        return 'PPM', 'image/x-portable-anymap'
    if len(data) >= 12 and data[:4] == b'FORM' and data[8:12] in (b'AIFF', b'AIFC'):
        return 'AIFF', 'audio/aiff'
    if data.startswith(b'.snd'):
        return 'AU', 'audio/basic'
    if len(data) >= 12 and data[4:8] == b'ftyp':
        box_size = int.from_bytes(data[:4], 'big')
        brands = [data[8:12]] + [data[i:i+4] for i in range(16, min(box_size, len(data), 4096) - 3, 4)]
        if any(brand in (b'avif', b'avis') for brand in brands):
            return 'AVIF', 'image/avif'
        if data[8:12] in (b'heic', b'heix', b'mif1', b'msf1'):
            return 'HEIF', 'image/heif'
        return 'MP4', 'video/mp4'
    if len(data) >= 262 and data[257:262] == b'ustar':
        return 'TAR', 'application/x-tar'
    if len(data) >= 2 and data[0] == 255 and data[1] & 0xe0 == 0xe0:
        return 'MPEG_AUDIO', 'audio/mpeg'
    return 'UNKNOWN', 'application/octet-stream'


def known_binary(data: bytes) -> bool:
    """Strong headers stay binary even when renamed to an allowed text suffix."""
    return sniff(data)[0] not in ('UNKNOWN', 'PE/DOS', 'BMP', 'MPEG_AUDIO')


def read_source(reader, relative: str) -> bytes:
    maximum = min(MAX_SOURCE, reader.settings['max_file_bytes'], reader.settings['max_scan_bytes'])
    chunks = []
    used = 0
    with reader.open_checked(reader.checked(relative), binary=True) as handle:
        if os.fstat(handle.fileno()).st_size > maximum:
            raise ValueError('FORMAT_SOURCE_LIMIT：來源超過解析上限；可改用 file_info 或 hash_files。')
        while True:
            checkpoint()
            chunk = handle.read(min(65536, maximum + 1 - used))
            if not chunk:
                break
            used += len(chunk)
            checkpoint(bytes_read=len(chunk))
            if used > maximum:
                raise ValueError('FORMAT_SOURCE_LIMIT：來源超過解析上限。')
            chunks.append(chunk)
    return b''.join(chunks)


def parse_snapshot(kind: str, data: bytes, **options) -> dict:
    """Fixed interpreter/script/operations. No source filename or user command."""
    checkpoint()
    if len(data) > MAX_SOURCE or kind not in ('document', 'media', 'archive', 'identify', 'image'):
        raise ValueError('FORMAT_INPUT_LIMIT：不支援的解析請求。')
    payload = json.dumps(dict(kind=kind, data=base64.b64encode(data).decode('ascii'), options=options)).encode()
    # Do not pass connection credentials, proxy settings, PYTHONPATH or user hooks.
    env = {key: value for key, value in os.environ.items()
           if key.upper() in ('SYSTEMROOT', 'WINDIR', 'TEMP', 'TMP', 'LANG')}
    command = [sys.executable, '-I', '-B', str(Path(__file__).with_name('format_worker.py').resolve())]
    flags = subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0
    started = time.monotonic()
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                          env=env, creationflags=flags, close_fds=True) as process:
        try:
            first = True
            while True:
                checkpoint()
                if time.monotonic() - started >= PARSER_TIMEOUT:
                    raise ValueError('FORMAT_TIMEOUT：格式解析超過期限。')
                try:
                    output, _ = process.communicate(payload if first else None, timeout=.1)
                    break
                except subprocess.TimeoutExpired:
                    first = False
            checkpoint()
            maximum = 5 * 1024 * 1024 if kind == 'image' else MAX_RESPONSE
            if process.returncode or len(output) > maximum:
                raise ValueError('FORMAT_RESOURCE_LIMIT：解析程序失敗或超出資源限制。')
            result = json.loads(output)
            if result.get('error'):
                raise ValueError(result['error'])
            return result
        finally:
            if process.poll() is None:
                process.kill()
            process.wait()


def basic_metadata(reader: Any, path: Path, info: os.stat_result,
                   handle: BinaryIO | None = None) -> dict:
    """共用列表與 file_info 的 metadata；ctime 永遠不當作 creation time。"""
    birth = getattr(info, 'st_birthtime', None)
    birth_ns = getattr(info, 'st_birthtime_ns', None)
    if birth is None and os.name == 'nt':
        # Python 3.10/3.11 沒有 st_birthtime，直接查詢 Windows FILETIME。
        if handle is None:
            with reader.open_checked(path, binary=True, metadata=True) as opened:
                return basic_metadata(reader, path, os.fstat(opened.fileno()), opened)
        import ctypes
        from ctypes import wintypes
        import msvcrt
        query = ctypes.WinDLL('kernel32', use_last_error=True).GetFileTime
        query.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.FILETIME),
                          ctypes.POINTER(wintypes.FILETIME), ctypes.POINTER(wintypes.FILETIME)]
        query.restype = wintypes.BOOL
        created = wintypes.FILETIME()
        if not query(msvcrt.get_osfhandle(handle.fileno()), ctypes.byref(created), None, None):
            raise ValueError('METADATA_UNAVAILABLE：無法取得 Windows creation time。')
        ticks = (created.dwHighDateTime << 32) | created.dwLowDateTime
        birth_ns = (ticks - 116444736000000000) * 100
        birth = birth_ns / 1000000000
    return dict(path=reader.relative(path), name=path.name, extension=path.suffix.lower(),
                size=info.st_size, created_time=birth, modified_time=info.st_mtime,
                accessed_time=info.st_atime, time_unit='Unix seconds UTC',
                created_time_ns=birth_ns, modified_time_ns=info.st_mtime_ns,
                accessed_time_ns=info.st_atime_ns,
                attributes=dict(mode=info.st_mode, windows=getattr(info, 'st_file_attributes', None),
                                links=info.st_nlink), links=info.st_nlink)


def file_info(reader, relative: str) -> dict:
    path = reader.checked(relative)
    source = None
    with reader.open_checked(path, binary=True, metadata=True) as handle:
        info = os.fstat(handle.fileno())
        metadata = basic_metadata(reader, path, info, handle)
        header = handle.read(min(HEADER_BYTES, reader.settings['max_file_bytes'], reader.settings['max_scan_bytes']))
        checkpoint(bytes_read=len(header))
        fmt, mime = sniff(header)
        maximum = min(MAX_SOURCE, reader.settings['max_file_bytes'], reader.settings['max_scan_bytes'])
        if (fmt == 'ZIP' or (fmt == 'UNKNOWN' and b'\0' not in header)) and info.st_size <= maximum:
            source = bytearray(header)
            while True:
                checkpoint()
                chunk = handle.read(min(65536, maximum + 1 - len(source)))
                if not chunk:
                    break
                checkpoint(bytes_read=len(chunk))
                source.extend(chunk)
                if len(source) > maximum:
                    raise ValueError('FORMAT_SOURCE_LIMIT：來源超過解析上限。')
    fmt, mime = sniff(header)
    detection = 'signature/header; content not yet validated'
    container_error = None
    if fmt == 'UNKNOWN' and info.st_size == len(header) and b'\0' not in header:
        try:
            header.decode('utf-8-sig')
        except UnicodeError:
            pass
        else:
            fmt, mime = 'UTF-8', 'text/plain'
            detection = 'complete small-file UTF-8 validation (no NUL)'
    if source is not None:
        try:
            identified = parse_snapshot('identify', source, excluded_names=sorted(reader._exclusions))
            fmt = identified['format']
            mime = {'DOCX': 'application/vnd.openxmlformats-officedocument.wordprocessingml.document',
                    'XLSX': 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet',
                    'PPTX': 'application/vnd.openxmlformats-officedocument.presentationml.presentation'}.get(fmt, mime)
            detection = identified.get('detection', 'validated ZIP directory and container type; document content not yet validated')
        except ValueError as exc:
            container_error = str(exc).split('：', 1)[0]
    mime = {'JSON': 'application/json', 'JSONL': 'application/x-ndjson', 'CSV': 'text/csv',
            'TSV': 'text/tab-separated-values', 'XML': 'application/xml', 'YAML': 'application/yaml',
            'EML': 'message/rfc822', 'MBOX': 'application/mbox', 'UTF-8': 'text/plain',
            'ODT': 'application/vnd.oasis.opendocument.text',
            'ODS': 'application/vnd.oasis.opendocument.spreadsheet',
            'ODP': 'application/vnd.oasis.opendocument.presentation', 'EPUB': 'application/epub+zip'}.get(fmt, mime)
    capabilities = ['file_info']
    within = info.st_size <= reader.settings['max_file_bytes']
    if within:
        capabilities.append('read_binary')
        if info.st_size <= reader.settings['max_scan_bytes']:
            capabilities.append('hash_files')
        if reader.is_text(path) and fmt in ('UNKNOWN', 'UTF-8'):
            capabilities.append('read_file (UTF-8 validation required)')
        if info.st_size <= min(MAX_SOURCE, reader.settings['max_scan_bytes']):
            if fmt in ('PDF', 'DOCX', 'XLSX', 'PPTX', 'ODT', 'ODS', 'ODP', 'EPUB', 'JSON', 'JSONL', 'CSV', 'TSV', 'XML', 'YAML', 'EML', 'MBOX', 'SRT', 'VTT') or (fmt == 'SQLITE' and format_limits()['sqlite_available']):
                capabilities.append('read_document (container validation required)')
            elif fmt == 'UTF-8':
                capabilities.append('read_document (format_hint and structure validation required)')
            if fmt in ('ZIP', 'DOCX', 'XLSX', 'PPTX', 'ODT', 'ODS', 'ODP', 'EPUB', 'TAR', 'GZIP'):
                capabilities.append('inspect_archive')
            from image_reader import IMAGE_EXTENSIONS
            if fmt in ('MP3', 'MPEG_AUDIO', 'FLAC', 'OGG', 'WAV', 'MP4', 'AVI', 'AIFF', 'AU', 'MATROSKA', 'PNG', 'JPEG', 'WEBP', 'GIF', 'BMP', 'TIFF', 'ICO', 'PPM') or (fmt == 'AVIF' and '.avif' in IMAGE_EXTENSIONS):
                capabilities.append('inspect_media')
        from image_reader import IMAGE_EXTENSIONS, MAX_SOURCE_BYTES
        if (fmt in IMAGE_EXTENSIONS.values() and IMAGE_EXTENSIONS.get(path.suffix.casefold()) == fmt
                and info.st_size <= min(MAX_SOURCE_BYTES, reader.settings['max_scan_bytes'])):
            capabilities.append('read_image (static decode required)')
    return dict(**metadata,
                format=fmt, mime_type=mime, detection=detection, container_error=container_error,
                capabilities=capabilities,
                content_within_root_limit=within, limits=format_limits())


def read_binary(reader, relative: str, offset: int, length: int) -> dict:
    if type(offset) is not int or not 0 <= offset <= 2**53 - 1 or type(length) is not int or not 1 <= length <= MAX_RANGE:
        raise ValueError('BINARY_RANGE：offset 或 length 超出範圍。')
    if length > reader.settings['max_scan_bytes']:
        raise ValueError('BINARY_RANGE：超過 root 累計讀取限制。')
    with reader.open_checked(reader.checked(relative), binary=True) as handle:
        size = os.fstat(handle.fileno()).st_size
        handle.seek(offset)
        data = handle.read(length)
        checkpoint(bytes_read=len(data))
    return dict(path=relative, offset=offset, returned_bytes=len(data), size=size,
                encoding='base64', data=base64.b64encode(data).decode('ascii'),
                has_more=offset + len(data) < size)


def inspect(reader, relative: str, kind: str, start: int = 0, limit: int = 50, **options) -> dict:
    if type(start) is not int or not 0 <= start <= 100000 or type(limit) is not int or not 1 <= limit <= 100:
        raise ValueError('FORMAT_RANGE：start 必須為 0 至 100000，limit 為 1 至 100。')
    if kind == 'archive':
        offset, length = options.get('offset', 0), options.get('length', 4096)
        if type(offset) is not int or not 0 <= offset <= 8388608 or type(length) is not int or not 1 <= length <= MAX_RANGE:
            raise ValueError('FORMAT_RANGE：成員 offset/length 超限。')
        if options.get('member_path') is not None and (start != 0 or limit != 50):
            raise ValueError('FORMAT_RANGE：成員模式不可同時指定目錄分頁。')
        if options.get('member_path') is None and (offset != 0 or length != 4096):
            raise ValueError('FORMAT_RANGE：offset/length 需要 member_path。')
    if options.get('format_hint') not in (None, 'CSV', 'TSV', 'JSON', 'JSONL', 'XML', 'YAML', 'INI', 'TOML', 'EML', 'MBOX', 'SRT', 'VTT'):
        raise ValueError('FORMAT_HINT：不支援的文字格式提示。')
    source = read_source(reader, relative)
    digest = hashlib.sha256(source).hexdigest()
    if options.get('expected_sha256') is not None and options['expected_sha256'] != digest:
        raise ValueError('FORMAT_SOURCE_CHANGED：來源 SHA-256 已變更，請重新開始分頁。')
    result = parse_snapshot(kind, source, start=start, limit=limit,
                            excluded_names=sorted(reader._exclusions), **options)
    result['path'] = relative
    result['source_sha256'] = digest
    return result
