"""Passive OpenDocument, EPUB and MIME readers; all resources are memory members."""
import email.policy
from email.parser import BytesParser
from html.parser import HTMLParser
import hashlib
import json
import re
import zipfile
from urllib.parse import unquote, urlsplit

from format_parsers import checked_zip, member, xml, local, safe_name, paginate, MAX_UNITS

ODF_MIMES = {
    'application/vnd.oasis.opendocument.text': 'ODT',
    'application/vnd.oasis.opendocument.spreadsheet': 'ODS',
    'application/vnd.oasis.opendocument.presentation': 'ODP',
}


def zip_kind(archive: zipfile.ZipFile) -> str:
    from format_parsers import office_kind
    names = archive.namelist()
    if '[Content_Types].xml' in names:
        return office_kind(archive)
    if 'mimetype' in names:
        mime = member(archive, 'mimetype').decode('ascii')
        if mime in ODF_MIMES and 'content.xml' in names:
            return ODF_MIMES[mime]
        if mime == 'application/epub+zip' and 'META-INF/container.xml' in names:
            return 'EPUB'
    return 'ZIP'


def passive_text(node: object) -> str:
    if local(node.tag).lower() in ('script', 'scripts', 'style', 'head', 'object', 'iframe', 'binary-data'):
        return ''
    return (node.text or '') + ''.join(passive_text(child) + (child.tail or '') for child in node)


def attribute(node: object, key: str, default: str = '') -> str:
    return next((value for name, value in node.attrib.items() if local(name) == key), default)


def odf_units(archive: zipfile.ZipFile, fmt: str) -> list:
    if 'META-INF/manifest.xml' in archive.namelist():
        manifest = xml(member(archive, 'META-INF/manifest.xml'))
        if any(local(node.tag) == 'encryption-data' for node in manifest.iter()):
            raise ValueError('DOCUMENT_ENCRYPTED：不讀取加密 OpenDocument。')
    tree = xml(member(archive, 'content.xml'))
    if local(tree.tag) != 'document-content':
        raise ValueError('DOCUMENT_FORMAT：OpenDocument 主文件結構無效。')
    units = []
    if fmt == 'ODT':
        for index, node in enumerate(n for n in tree.iter() if local(n.tag) in ('p', 'h')):
            units.append((dict(member='content.xml', paragraph=index + 1), passive_text(node)))
    elif fmt == 'ODP':
        for index, node in enumerate(n for n in tree.iter() if local(n.tag) == 'page'):
            units.append((dict(member='content.xml', slide=index + 1),
                          '\n'.join(passive_text(p) for p in node.iter() if local(p.tag) in ('p', 'h'))))
    else:
        for table in (node for node in tree.iter() if local(node.tag) == 'table'):
            name = attribute(table, 'name')
            if len(name) > 256:
                raise ValueError('DOCUMENT_LIMIT：工作表名稱超限。')
            row_index = 0
            for row in (node for node in table.iter() if local(node.tag) == 'table-row'):
                repeat = int(attribute(row, 'number-rows-repeated', '1'))
                if not 1 <= repeat <= 1000:
                    raise ValueError('DOCUMENT_LIMIT：重複資料列超限。')
                cells = []
                for cell in row:
                    if local(cell.tag) not in ('table-cell', 'covered-table-cell'):
                        continue
                    amount = int(attribute(cell, 'number-columns-repeated', '1'))
                    if not 1 <= amount <= 256 or len(cells) + amount > 256:
                        raise ValueError('DOCUMENT_LIMIT：重複欄位超限。')
                    value = dict(value_type=attribute(cell, 'value-type'), value=attribute(cell, 'value'),
                                 date_value=attribute(cell, 'date-value'), boolean_value=attribute(cell, 'boolean-value'),
                                 formula=attribute(cell, 'formula'), text=passive_text(cell))
                    cells.extend([value] * amount)
                rendered = json.dumps(cells, ensure_ascii=False)
                for _ in range(repeat):
                    row_index += 1
                    if len(units) >= MAX_UNITS:
                        raise ValueError('DOCUMENT_LIMIT：OpenDocument 列數超限。')
                    units.append((dict(member='content.xml', sheet=name, row=row_index), rendered))
    return units


def epub_path(base: str, href: str) -> str:
    url = urlsplit(href)
    if url.scheme or url.netloc or url.query or url.path.startswith('/'):
        raise ValueError('DOCUMENT_EXTERNAL：EPUB 必要資源不可以是外部網址。')
    name = base + unquote(url.path, errors='strict')
    safe_name(name)
    return name


def epub_units(archive: zipfile.ZipFile) -> list:
    if 'META-INF/encryption.xml' in archive.namelist():
        raise ValueError('DOCUMENT_ENCRYPTED：不讀取加密／混淆 EPUB。')
    container = xml(member(archive, 'META-INF/container.xml'))
    paths = [n.attrib.get('full-path', '') for n in container.iter() if local(n.tag) == 'rootfile']
    if local(container.tag) != 'container' or len(paths) != 1:
        raise ValueError('DOCUMENT_FORMAT：EPUB rootfile 無效或多重。')
    package_path = epub_path('', paths[0])
    package = xml(member(archive, package_path))
    if local(package.tag) != 'package':
        raise ValueError('DOCUMENT_FORMAT：EPUB package 無效。')
    base = package_path.rsplit('/', 1)[0] + '/' if '/' in package_path else ''
    items = {}
    for node in package.iter():
        if local(node.tag) == 'item':
            key = node.attrib.get('id')
            if not key or key in items:
                raise ValueError('DOCUMENT_FORMAT：EPUB manifest id 重複或缺少。')
            items[key] = node
    units = []
    seen = set()
    for chapter, item in enumerate(n for n in package.iter() if local(n.tag) == 'itemref'):
        key = item.attrib.get('idref')
        node = items.get(key)
        if node is None or key in seen or node.attrib.get('media-type') != 'application/xhtml+xml':
            raise ValueError('DOCUMENT_FORMAT：EPUB spine 無效或不是 XHTML。')
        seen.add(key)
        target = epub_path(base, node.attrib.get('href', ''))
        tree = xml(member(archive, target))
        if local(tree.tag) != 'html':
            raise ValueError('DOCUMENT_FORMAT：EPUB 章節不是 XHTML。')
        for index, body in enumerate(n for n in tree.iter() if local(n.tag) == 'body'):
            units.append((dict(member=target, chapter=chapter + 1, body=index + 1), passive_text(body)))
    if not seen:
        raise ValueError('DOCUMENT_FORMAT：EPUB 缺少 spine。')
    return units


def zip_document(data: bytes, options: dict) -> dict:
    from format_parsers import office_units
    with checked_zip(data) as archive:
        archive._reader_exclusions = options['excluded_names']
        fmt = zip_kind(archive)
        if fmt in ('DOCX', 'XLSX', 'PPTX'):
            units = office_units(archive, fmt, options['excluded_names'])
        elif fmt in ('ODT', 'ODS', 'ODP'):
            units = odf_units(archive, fmt)
        elif fmt == 'EPUB':
            units = epub_units(archive)
        else:
            raise ValueError('DOCUMENT_FORMAT：不支援的 ZIP 文件。')
        return dict(format=fmt, **paginate(units, options['start'], options['limit']))


class PassiveHTML(HTMLParser):
    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.parts = []
        self.suppressed = []
        self.nodes = 0

    def handle_starttag(self, tag: str, attrs: list) -> None:
        self.nodes += 1
        if self.nodes > MAX_UNITS or len(self.suppressed) > 64:
            raise ValueError('DOCUMENT_HTML_LIMIT：HTML 節點或深度超限。')
        if tag in ('script', 'style', 'head', 'object', 'iframe'):
            self.suppressed.append(tag)
        elif not self.suppressed and tag in ('p', 'br', 'div', 'tr', 'li'):
            self.parts.append('\n')

    def handle_endtag(self, tag: str) -> None:
        if self.suppressed and self.suppressed[-1] == tag:
            self.suppressed.pop()

    def handle_data(self, data: str) -> None:
        if not self.suppressed:
            self.parts.append(data)


def email_document(data: bytes, fmt: str, options: dict) -> dict:
    if fmt == 'MBOX':
        # mboxo/mboxrd envelopes only; ambiguous Content-Length variants are rejected.
        boundaries = list(re.finditer(rb'(?m)^From [^\s]+ [^\r\n]+\d{4}\r?$', data))
        if not boundaries or boundaries[0].start() != 0 or len(boundaries) > 1000:
            raise ValueError('DOCUMENT_MBOX：MBOX envelope 無效或郵件數超限。')
        messages = [(match.end() + 1, data[match.end() + 1:boundaries[i+1].start() if i+1 < len(boundaries) else len(data)])
                    for i, match in enumerate(boundaries)]
    else:
        messages = [(0, data)]
    units = []
    for number, (offset, raw) in enumerate(messages, 1):
        if len(raw) > 8 * 1024 * 1024:
            raise ValueError('DOCUMENT_MAIL_LIMIT：單封郵件超過 8 MiB。')
        message = BytesParser(policy=email.policy.default).parsebytes(raw)
        if not any(message.get(key) for key in ('From', 'Subject', 'MIME-Version')) or message.get('Content-Length'):
            raise ValueError('DOCUMENT_MAIL：郵件 header 無效或不支援 Content-Length MBOX。')
        headers = {key: str(message.get(key, '')) for key in ('From', 'To', 'Cc', 'Subject', 'Date', 'Message-ID')}
        units.append((dict(message=number, byte_offset=offset, section='headers'), json.dumps(headers, ensure_ascii=False)))
        stack = [(message, '1', 0)]
        count = 0
        while stack:
            part, path, depth = stack.pop()
            count += 1
            if count > 1000 or depth > 32 or part.defects:
                raise ValueError('DOCUMENT_MAIL_LIMIT：MIME 損毀、深度或項目數超限。')
            disposition = part.get_content_disposition()
            content_type = part.get_content_type()
            if part.is_multipart() and content_type != 'message/rfc822' and disposition != 'attachment':
                children = part.get_payload()
                stack.extend((child, path + '.' + str(i+1), depth+1) for i, child in reversed(list(enumerate(children))))
                continue
            location = dict(message=number, byte_offset=offset, part=path, content_type=content_type)
            if part.is_multipart():
                units.append((location, json.dumps(dict(attachment=True, content='nested message not traversed'))))
                continue
            payload = part.get_payload(decode=True) or b''
            if part.defects:
                raise ValueError('DOCUMENT_MAIL：MIME 編碼損毀。')
            if disposition == 'attachment' or content_type not in ('text/plain', 'text/html'):
                units.append((location, json.dumps(dict(attachment=True, filename=part.get_filename(),
                    size=len(payload), sha256=hashlib.sha256(payload).hexdigest()), ensure_ascii=False)))
                continue
            charset = (part.get_content_charset() or 'ascii').lower()
            allowed = ('utf-8', 'us-ascii', 'ascii', 'iso-8859-1', 'windows-1252', 'big5', 'cp950', 'gb18030', 'shift_jis', 'iso-2022-jp')
            if charset not in allowed:
                raise ValueError('DOCUMENT_MAIL_CHARSET：不支援的郵件文字編碼。')
            text = payload.decode(charset, errors='strict')
            if '\0' in text:
                raise ValueError('DOCUMENT_MAIL：郵件文字含 NUL。')
            if fmt == 'MBOX':
                text = re.sub(r'(?m)^>(>*From )', r'\1', text)
            if content_type == 'text/html':
                parser = PassiveHTML()
                parser.feed(text)
                parser.close()
                text = ''.join(parser.parts)
            units.append((location, text))
        if len(units) > MAX_UNITS:
            raise ValueError('DOCUMENT_MAIL_LIMIT：郵件輸出項目超限。')
    return dict(format=fmt, messages=len(messages), unit='text segment (zero-based index)',
                **paginate(units, options['start'], options['limit']))
