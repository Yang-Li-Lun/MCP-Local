"""Parsers for in-memory snapshots. Called only inside the limited worker."""
import gzip
import io
import json
import re
import stat
import struct
import tarfile
from pathlib import PurePosixPath, PureWindowsPath
import xml.etree.ElementTree as ET
import zipfile
import zlib
import base64
import hashlib

from format_reader import sniff, MAX_RESPONSE

MAX_ENTRIES = 4096
MAX_MEMBER = 8 * 1024 * 1024
MAX_EXPANDED = 128 * 1024 * 1024
MAX_RATIO = 200
MAX_UNITS = 100000


def safe_name(name: str) -> None:
    win = PureWindowsPath(name)
    parts = name.rstrip('/').split('/')
    if (not name or len(name) > 1024 or win.drive or win.root or '\\' in name
            or any(p in ('', '.', '..') or p.endswith((' ', '.')) or PureWindowsPath(p).is_reserved()
                   or any(ord(c) < 32 or c in ':*?"<>|' for c in p) for p in parts)):
        raise ValueError('ARCHIVE_PATH：封存含不安全或含糊路徑。')


def excluded(name: str, names: list) -> bool:
    return any(p.startswith('.') or p.casefold() in names for p in PurePosixPath(name).parts)


def zip_directory(entry: zipfile.ZipInfo) -> bool:
    return bool(entry.is_dir() or entry.external_attr & 0x10 or
                stat.S_IFMT(entry.external_attr >> 16) == stat.S_IFDIR)


def checked_zip(data: bytes) -> zipfile.ZipFile:
    # Bound central-directory allocation BEFORE ZipFile constructs ZipInfo objects.
    eocd = data.rfind(b'PK\x05\x06', max(0, len(data) - 65557))
    if eocd < 0 or eocd + 22 > len(data):
        raise ValueError('ARCHIVE_INVALID：缺少 ZIP 目錄。')
    disk, cd_disk, count_disk, count, size, offset, comment = struct.unpack_from('<4H2IH', data, eocd + 4)
    if (disk or cd_disk or count_disk != count or count > MAX_ENTRIES or count == 65535
            or size > 4*1024*1024 or offset + size != eocd or eocd + 22 + comment != len(data)):
        raise ValueError('ARCHIVE_LIMIT：ZIP64、多磁碟、附加資料或目錄超限。')
    archive = zipfile.ZipFile(io.BytesIO(data))
    try:
        entries = archive.infolist()
        if len(entries) != count:
            raise ValueError('ARCHIVE_INVALID：目錄項目數不符。')
        total = 0
        seen = set()
        spans = []
        for entry in entries:
            safe_name(entry.orig_filename)
            safe_name(entry.filename)
            key = entry.filename.rstrip('/').casefold()
            mode = entry.external_attr >> 16
            if (key in seen or stat.S_IFMT(mode) not in (0, stat.S_IFREG, stat.S_IFDIR)
                    or entry.external_attr & 0x400):
                raise ValueError('ARCHIVE_LINK：拒絕重複名稱、連結及特殊項目。')
            seen.add(key)
            if entry.flag_bits & 1 or entry.compress_type not in (0, 8):
                raise ValueError('ARCHIVE_METHOD：不支援加密或此壓縮方法。')
            total += entry.file_size
            if (entry.file_size > MAX_MEMBER or total > MAX_EXPANDED
                    or entry.file_size > max(1, entry.compress_size) * MAX_RATIO):
                raise ValueError('ARCHIVE_LIMIT：宣告展開量或壓縮比例超限。')
            pos = entry.header_offset
            if pos < 0 or pos + 30 > offset or data[pos:pos+4] != b'PK\x03\x04':
                raise ValueError('ARCHIVE_INVALID：成員 header 無效。')
            flags, method = struct.unpack_from('<HH', data, pos + 6)
            name_size, extra_size = struct.unpack_from('<HH', data, pos + 26)
            end = pos + 30 + name_size + extra_size + entry.compress_size
            if flags != entry.flag_bits or method != entry.compress_type or end > offset:
                raise ValueError('ARCHIVE_INVALID：成員界線無效。')
            local_name = data[pos+30:pos+30+name_size].decode('utf-8' if flags & 0x800 else 'cp437')
            if local_name != entry.orig_filename or '\x00' in local_name:
                raise ValueError('ARCHIVE_INVALID：成員名稱不一致。')
            crc, compressed, expanded = struct.unpack_from('<III', data, pos + 14)
            if not flags & 8 and (crc, compressed, expanded) != (entry.CRC, entry.compress_size, entry.file_size):
                raise ValueError('ARCHIVE_INVALID：本機與目錄長度／CRC 不一致。')
            if flags & 8:
                descriptor = end + (4 if data[end:end+4] == b'PK\x07\x08' else 0)
                if descriptor + 12 > offset or struct.unpack_from('<III', data, descriptor) != (entry.CRC, entry.compress_size, entry.file_size):
                    raise ValueError('ARCHIVE_INVALID：資料描述符無效。')
                end = descriptor + 12
            spans.append((pos, end))
        spans.sort()
        if any(a[1] > b[0] for a, b in zip(spans, spans[1:])):
            raise ValueError('ARCHIVE_OVERLAP：成員重疊。')
        archive._snapshot = data
        # ZIP producers may mark directories by DOS/Unix attributes without '/'.
        # Honor Hidden ancestors independently of filename casing or producer.
        archive._hidden_prefixes = tuple(
            e.filename.rstrip('/').casefold() + '/' for e in entries
            if e.external_attr & 2 and zip_directory(e))
        return archive
    except BaseException:
        archive.close()
        raise


def member(archive: zipfile.ZipFile, name: str) -> bytes:
    if excluded(name, getattr(archive, '_reader_exclusions', [])):
        raise ValueError('DOCUMENT_EXCLUDED：必要文件成員已排除。')
    info = archive.getinfo(name)
    if zip_directory(info):
        raise ValueError('ARCHIVE_MEMBER：目錄不是一般成員檔案。')
    if info.external_attr & 2 or name.casefold().startswith(archive._hidden_prefixes):
        raise ValueError('DOCUMENT_EXCLUDED：必要文件成員具有 Hidden 屬性。')
    if info.file_size > MAX_MEMBER:
        raise ValueError('DOCUMENT_LIMIT：XML 成員過大。')
    snapshot = archive._snapshot
    name_size, extra_size = struct.unpack_from('<HH', snapshot, info.header_offset + 26)
    start = info.header_offset + 30 + name_size + extra_size
    compressed = snapshot[start:start + info.compress_size]
    if info.compress_type == 8:
        decoder = zlib.decompressobj(-15)
        data = decoder.decompress(compressed, MAX_MEMBER + 1)
        if not decoder.eof or decoder.unused_data or decoder.unconsumed_tail:
            raise ValueError('ARCHIVE_INVALID：壓縮串流未完整結束或含額外資料。')
    else:
        data = compressed
    if len(data) > MAX_MEMBER or len(data) != info.file_size:
        raise ValueError('DOCUMENT_LIMIT：實際展開量超限。')
    if zlib.crc32(data) != info.CRC:
        raise ValueError('ARCHIVE_CRC：成員 CRC 不符。')
    return data


def xml(data: bytes) -> ET.Element:
    # OOXML UTF-8 only; reject DTD/entities before any XML parsing.
    text = data.decode('utf-8-sig')
    if re.search(r'<!\s*(DOCTYPE|ENTITY)', text, re.I):
        raise ValueError('DOCUMENT_XML：禁止 DTD 與 entity。')
    depth = count = 0
    root = None
    for event, element in ET.iterparse(io.StringIO(text), events=('start', 'end')):
        if event == 'start':
            depth += 1
            count += 1
            if root is None:
                root = element
            if depth > 64 or count > 200000:
                raise ValueError('DOCUMENT_XML_LIMIT：XML 深度或節點超限。')
        else:
            depth -= 1
    if root is None:
        raise ValueError('DOCUMENT_XML：空白 XML。')
    return root


def local(tag: str) -> str:
    return tag.rsplit('}', 1)[-1]


def text_nodes(element: ET.Element) -> str:
    return ''.join(e.text or '' for e in element.iter() if local(e.tag) == 't')


def office_kind(archive: zipfile.ZipFile) -> str:
    types = xml(member(archive, '[Content_Types].xml'))
    if local(types.tag) != 'Types':
        raise ValueError('DOCUMENT_FORMAT：Content Types 結構無效。')
    values = {e.attrib.get('PartName'): e.attrib.get('ContentType') for e in types}
    candidates = [('DOCX', '/word/document.xml', 'application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml'),
                  ('XLSX', '/xl/workbook.xml', 'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml'),
                  ('PPTX', '/ppt/presentation.xml', 'application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml')]
    found = [kind for kind, part, mime in candidates if values.get(part) == mime and part[1:] in archive.namelist()]
    if len(found) != 1:
        raise ValueError('DOCUMENT_FORMAT：不是支援的 DOCX/XLSX/PPTX 主文件。')
    return found[0]


def relations(archive: zipfile.ZipFile, name: str, base: str, names: list) -> dict:
    result = {}
    tree = xml(member(archive, name))
    if local(tree.tag) != 'Relationships':
        raise ValueError('DOCUMENT_FORMAT：關聯結構無效。')
    for item in tree:
        if item.attrib.get('TargetMode') == 'External':
            continue
        target = item.attrib.get('Target', '')
        # Accept only members below the owning document's directory.
        target = target.lstrip('/') if target.startswith('/' + base + '/') else base + '/' + target
        safe_name(target)
        if not excluded(target, names):
            result[item.attrib.get('Id')] = target
    return result


def office_units(archive: zipfile.ZipFile, kind: str, names: list):
    if kind == 'DOCX':
        if excluded('word/document.xml', names):
            raise ValueError('DOCUMENT_EXCLUDED：主文件已排除。')
        tree = xml(member(archive, 'word/document.xml'))
        if local(tree.tag) != 'document':
            raise ValueError('DOCUMENT_FORMAT：DOCX 主文件結構無效。')
        for index, paragraph in enumerate(e for e in tree.iter() if local(e.tag) == 'p'):
            yield dict(paragraph=index + 1), text_nodes(paragraph)
    elif kind == 'XLSX':
        strings = []
        if 'xl/sharedStrings.xml' in archive.namelist():
            if excluded('xl/sharedStrings.xml', names):
                raise ValueError('DOCUMENT_EXCLUDED：字串表已排除。')
            strings = [text_nodes(e) for e in xml(member(archive, 'xl/sharedStrings.xml')) if local(e.tag) == 'si']
        rels = relations(archive, 'xl/_rels/workbook.xml.rels', 'xl', names)
        workbook = xml(member(archive, 'xl/workbook.xml'))
        if local(workbook.tag) != 'workbook':
            raise ValueError('DOCUMENT_FORMAT：XLSX 主文件結構無效。')
        for sheet in workbook.iter():
            if local(sheet.tag) != 'sheet':
                continue
            rid = next((v for k, v in sheet.attrib.items() if local(k) == 'id'), None)
            target = rels.get(rid)
            if target is None:
                continue
            worksheet = xml(member(archive, target))
            if local(worksheet.tag) != 'worksheet':
                raise ValueError('DOCUMENT_FORMAT：工作表結構無效。')
            for row in worksheet.iter():
                if local(row.tag) != 'row':
                    continue
                cells = []
                for cell in row:
                    if local(cell.tag) != 'c':
                        continue
                    value = next((e.text or '' for e in cell if local(e.tag) == 'v'), '')
                    if cell.attrib.get('t') == 's':
                        index = int(value)
                        if not 0 <= index < len(strings):
                            raise ValueError('DOCUMENT_FORMAT：共用字串索引無效。')
                        value = strings[index]
                    elif cell.attrib.get('t') == 'inlineStr':
                        value = text_nodes(cell)
                    formula = next((e.text for e in cell if local(e.tag) == 'f'), None)
                    cells.append(dict(cell=cell.attrib.get('r'), value=value, formula=formula))
                yield dict(sheet=sheet.attrib.get('name', '')[:200], row=row.attrib.get('r', '')[:20]), json.dumps(cells, ensure_ascii=False)
    else:
        rels = relations(archive, 'ppt/_rels/presentation.xml.rels', 'ppt', names)
        slide = 0
        presentation = xml(member(archive, 'ppt/presentation.xml'))
        if local(presentation.tag) != 'presentation':
            raise ValueError('DOCUMENT_FORMAT：PPTX 主文件結構無效。')
        for item in presentation.iter():
            if local(item.tag) != 'sldId':
                continue
            rid = next((v for k, v in item.attrib.items() if k.endswith('}id')), None)
            target = rels.get(rid)
            if target is None:
                continue
            slide += 1
            tree = xml(member(archive, target))
            if local(tree.tag) != 'sld':
                raise ValueError('DOCUMENT_FORMAT：投影片結構無效。')
            yield dict(slide=slide), '\n'.join(text_nodes(e) for e in tree.iter() if local(e.tag) == 'p')


def paginate(units, start: int, limit: int) -> dict:
    rows = []
    count = 0
    used = 0
    for location, text in units:
        for offset in range(0, max(1, len(text)), 2000):
            if count >= MAX_UNITS:
                raise ValueError('DOCUMENT_UNITS_LIMIT：文件片段數超限。')
            row = dict(index=count, location=location, char_offset=offset, text=text[offset:offset+2000])
            if count >= start and len(rows) < limit:
                cost = len(json.dumps(row, ensure_ascii=False).encode('utf-8'))
                if cost > 128 * 1024:
                    raise ValueError('DOCUMENT_OUTPUT_LIMIT：單一來源位置超過分頁預算。')
                if used + cost > 128 * 1024:
                    return dict(units=rows, returned_count=len(rows), next_start=count, has_more=True,
                                total_units=None, response_limited=True)
                rows.append(row)
                used += cost
            elif count >= start + limit:
                return dict(units=rows, returned_count=len(rows), next_start=count, has_more=True,
                            total_units=None, response_limited=False)
            count += 1
    return dict(units=rows, returned_count=len(rows), next_start=None, has_more=False,
                total_units=count, response_limited=False)


def document(data: bytes, options: dict) -> dict:
    fmt, _ = sniff(data)
    if options.get('format_hint') is not None and fmt != 'UNKNOWN':
        raise ValueError('DOCUMENT_FORMAT：文字提示不符合二進位 header。')
    if options.get('table') is not None and fmt != 'SQLITE':
        raise ValueError('DOCUMENT_FORMAT：table 僅適用 SQLite。')
    if fmt == 'PDF':
        from pypdf import PdfReader
        pdf = PdfReader(io.BytesIO(data), strict=True)
        if pdf.is_encrypted:
            raise ValueError('DOCUMENT_ENCRYPTED：不讀取加密 PDF。')
        if len(pdf.pages) > 2000:
            raise ValueError('DOCUMENT_PAGES_LIMIT：PDF 超過 2000 頁。')

        def pages():
            for number, page in enumerate(pdf.pages, 1):
                contents = page.get_contents()
                if contents is not None and len(contents.get_data()) > MAX_MEMBER:
                    raise ValueError('DOCUMENT_STREAM_LIMIT：PDF 頁面展開量超限。')
                yield dict(page=number), page.extract_text() or ''
        result = paginate(pages(), options['start'], options['limit'])
        result['pages'] = len(pdf.pages)
    elif fmt == 'ZIP':
        from document_extras import zip_document
        result = zip_document(data, options)
        fmt = result.pop('format')
    elif fmt == 'SQLITE':
        from structured_parser import sqlite_document
        return sqlite_document(data, options)
    elif fmt == 'UNKNOWN':
        from structured_parser import detect_text, structured, TEXT_FORMATS
        fmt = options.get('format_hint') or detect_text(data)
        if fmt not in TEXT_FORMATS:
            raise ValueError('DOCUMENT_FORMAT：無法辨識結構化文字；CSV/TSV/TOML/INI 等可指定 format_hint。')
        if fmt in ('EML', 'MBOX'):
            from document_extras import email_document
            return email_document(data, fmt, options)
        return structured(data, fmt, options)
    else:
        raise ValueError('DOCUMENT_FORMAT：此內容格式尚不支援結構讀取；可改用 file_info、hash_files 或 read_binary。')
    return dict(format=fmt, unit='text segment (zero-based index)', external_resources='never accessed',
                active_content='not executed', **result)


def archive_info(data: bytes, options: dict) -> dict:
    fmt, _ = sniff(data)
    rows = []
    skipped = 0
    total = 0
    requested = options.get('member_path')
    contents = None
    if requested is not None:
        safe_name(requested)
        if excluded(requested, options['excluded_names']):
            raise ValueError('ARCHIVE_EXCLUDED：成員已排除。')
    if fmt == 'ZIP':
        with checked_zip(data) as archive:
            archive._reader_exclusions = options['excluded_names']
            for e in archive.infolist():
                total += e.file_size
                if excluded(e.filename, options['excluded_names']) or e.external_attr & 2 or e.filename.casefold().startswith(archive._hidden_prefixes):
                    skipped += 1
                    continue
                rows.append(dict(name=e.filename, size=e.file_size, compressed_size=e.compress_size,
                                 compression='stored' if e.compress_type == 0 else 'deflate',
                                 directory=zip_directory(e), crc32=f'{e.CRC:08x}'))
                if e.filename == requested and not zip_directory(e):
                    contents = member(archive, e.filename)
    else:
        if fmt == 'GZIP':
            with gzip.GzipFile(fileobj=io.BytesIO(data)) as source:
                unpacked = source.read(32 * 1024 * 1024 + 1)
            if len(unpacked) > 32*1024*1024 or len(unpacked) > max(1, len(data))*MAX_RATIO:
                raise ValueError('ARCHIVE_LIMIT：GZIP 展開量超限。')
            data = unpacked
            fmt = 'TAR.GZ'
        elif fmt != 'TAR':
            raise ValueError('ARCHIVE_FORMAT：只支援 ZIP、TAR、TAR.GZ。')
        seen = set()
        with tarfile.open(fileobj=io.BytesIO(data), mode='r:') as archive:
            for i, e in enumerate(archive):
                if i >= MAX_ENTRIES:
                    raise ValueError('ARCHIVE_LIMIT：項目數超限。')
                safe_name(e.name)
                key = e.name.rstrip('/').casefold()
                if not (e.isfile() or e.isdir()) or e.issparse() or key in seen:
                    raise ValueError('ARCHIVE_LINK：拒絕連結、稀疏檔案、特殊項目及重複名稱。')
                seen.add(key)
                total += e.size
                if e.size < 0 or e.size > MAX_MEMBER or total > MAX_EXPANDED or e.offset_data + e.size > len(data):
                    raise ValueError('ARCHIVE_LIMIT：成員容量或界線超限。')
                if excluded(e.name, options['excluded_names']):
                    skipped += 1
                    continue
                rows.append(dict(name=e.name, size=e.size, directory=e.isdir(), compressed_size=None,
                                 compression='gzip' if fmt == 'TAR.GZ' else 'stored'))
                if e.name == requested and e.isfile():
                    contents = data[e.offset_data:e.offset_data + e.size]
    if requested is not None:
        if contents is None:
            raise ValueError('ARCHIVE_MEMBER：找不到可讀取的一般成員。')
        offset, length = options.get('offset', 0), options.get('length', 4096)
        chunk = contents[offset:offset + length]
        return dict(format=fmt, member_path=requested, size=len(contents), offset=offset,
                    returned_bytes=len(chunk), encoding='base64', data=base64.b64encode(chunk).decode('ascii'),
                    sha256=hashlib.sha256(contents).hexdigest(), has_more=offset + len(chunk) < len(contents),
                    next_offset=offset + len(chunk) if offset + len(chunk) < len(contents) else None,
                    member_contents_validated=True, integrity='CRC32 and exact length' if fmt == 'ZIP' else 'TAR header checksum and bounds; TAR has no payload checksum',
                    nested_archives='opaque bytes; never recursively opened')
    start, limit = options['start'], options['limit']
    page = []
    used = 0
    for row in rows[start:start+limit]:
        cost = len(json.dumps(row, ensure_ascii=False).encode('utf-8'))
        if used + cost > 128 * 1024:
            break
        page.append(row)
        used += cost
    more = start + len(page) < len(rows)
    return dict(format=fmt, entries=page, returned_count=len(page), total_entries=len(rows),
                skipped_entries=skipped, declared_expanded_bytes=total, has_more=more,
                next_start=start+len(page) if more else None, member_contents_validated=False,
                nested_archives='opaque entries; never recursively opened')
