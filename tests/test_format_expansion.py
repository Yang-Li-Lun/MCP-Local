"""Real snapshot workers: formats, continuation, corrupt tails and hostile inputs."""
import asyncio
import base64
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import sqlite3
import stat
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from email.message import EmailMessage
from unittest.mock import patch

from PIL import Image, features
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

from format_reader import parse_snapshot, MAX_RESPONSE
from structured_parser import sqlite_supported
from workspace_reader import WorkspaceReader
from tool_contract import verify_contract, SERVICE_VERSION, CONTRACT_VERSION


def zip_bytes(entries, compression=zipfile.ZIP_STORED):
    out = io.BytesIO()
    with zipfile.ZipFile(out, 'w', compression=compression) as archive:
        for name, value in entries:
            archive.writestr(name, value)
    return out.getvalue()


def tar_bytes(entries):
    out = io.BytesIO()
    with tarfile.open(fileobj=out, mode='w') as archive:
        for name, value in entries:
            item = tarfile.TarInfo(name)
            item.size = len(value)
            archive.addfile(item, io.BytesIO(value))
    return out.getvalue()


def odf_bytes(kind, content):
    mime = {'ODT': 'text', 'ODS': 'spreadsheet', 'ODP': 'presentation'}[kind]
    return zip_bytes([('mimetype', 'application/vnd.oasis.opendocument.' + mime),
                      ('content.xml', '<document-content>' + content + '</document-content>')])


def epub_bytes(href='chapter.xhtml', chapter=None):
    return zip_bytes([
        ('mimetype', 'application/epub+zip'),
        ('META-INF/container.xml', '<container><rootfiles><rootfile full-path="OEBPS/book.opf"/></rootfiles></container>'),
        ('OEBPS/book.opf', '<package><manifest><item id="a" href="' + href + '" media-type="application/xhtml+xml"/></manifest><spine><itemref idref="a"/></spine></package>'),
        ('OEBPS/chapter.xhtml', chapter or '<html><body><p>chapter marker</p><script>NEVER EXECUTE</script><img src="https://invalid.example/x"/></body></html>')])


def database_bytes(sql='CREATE TABLE sample(id INTEGER PRIMARY KEY, name TEXT, payload BLOB)'):
    db = sqlite3.connect(':memory:')
    try:
        db.execute(sql)
        if sql.startswith('CREATE TABLE sample'):
            db.executemany('INSERT INTO sample VALUES(?,?,?)', [(i, f'row {i}', bytes([i % 256])) for i in range(200)])
        db.commit()
        return db.serialize()
    finally:
        db.close()


class ExpansionTests(unittest.TestCase):
    def setUp(self):
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.root = Path(folder.name).resolve()
        self.workspace = WorkspaceReader([dict(id='main', path=str(self.root))], 'main', {'max_file_bytes': 32*1024*1024})

    def put(self, data, name='sample.data'):
        (self.root / name).write_bytes(data)
        return name

    def document(self, data, **options):
        return self.workspace.read_document(self.put(data), **options)

    def rejected(self, data, **options):
        with self.assertRaises(ValueError):
            self.document(data, **options)

    def test_archive_ranges_full_hash_crc_and_no_extract(self):
        payload = bytes(range(256)) * 100
        for make in (zip_bytes, tar_bytes, lambda entries: gzip.compress(tar_bytes(entries))):
            data = make([('folder/item.bin', payload)])
            name = self.put(data)
            before = (set(self.root.iterdir()), (self.root / name).stat().st_mtime_ns)
            first = self.workspace.inspect_archive(name, member_path='folder/item.bin', length=123)
            self.assertEqual(base64.b64decode(first['data']), payload[:123])
            self.assertEqual(first['sha256'], hashlib.sha256(payload).hexdigest())
            second = self.workspace.inspect_archive(name, member_path='folder/item.bin', offset=123,
                                                     expected_sha256=first['source_sha256'])
            self.assertEqual(base64.b64decode(second['data']), payload[123:4219])
            self.assertTrue(second['member_contents_validated'])
            self.assertEqual(before, (set(self.root.iterdir()), (self.root / name).stat().st_mtime_ns))
            self.assertEqual((self.root / name).read_bytes(), data)

    def test_archive_deflate_and_nested_opaque(self):
        inner = zip_bytes([('../unsafe', b'opaque')])
        name = self.put(zip_bytes([('inner.zip', inner)], zipfile.ZIP_DEFLATED))
        result = self.workspace.inspect_archive(name, member_path='inner.zip')
        self.assertEqual(base64.b64decode(result['data']), inner)
        self.assertIn('never recursively', result['nested_archives'])

    def test_archive_unsafe_names_collisions_links(self):
        cases = [[(name, b'x')] for name in ('../escape', '/abs', 'C:/abs', 'a\\b', 'a/../b', 'NUL', 'a:stream')]
        cases += [[('a', b'x'), ('A', b'y')], [('a', b'x'), ('a', b'y')]]
        link = zipfile.ZipInfo('link')
        link.create_system = 3
        link.external_attr = (stat.S_IFLNK | 0o777) << 16
        cases.append([(link, b'target')])
        for entries in cases:
            name = self.put(zip_bytes(entries))
            with self.subTest(entries=str(entries)[:60]), self.assertRaises(ValueError):
                self.workspace.inspect_archive(name, member_path='a')

    def test_archive_hidden_excluded_and_missing_members(self):
        hidden = zipfile.ZipInfo('hidden')
        hidden.external_attr = 2
        folder = zipfile.ZipInfo('Private/')
        folder.external_attr = 2
        dos_folder = zipfile.ZipInfo('DOS')
        dos_folder.external_attr = 0x12
        name = self.put(zip_bytes([('.private', b'x'), ('secrets.json', b'x'), (hidden, b'x'), ('folder/', b''), (folder, b''), ('private/child', b'x'), (dos_folder, b''), ('dos/child', b'x')]))
        for member in ('.private', 'secrets.json', 'hidden', 'folder/', 'missing', 'private/child', 'dos/child'):
            with self.subTest(member=member), self.assertRaises(ValueError):
                self.workspace.inspect_archive(name, member_path=member)

    def test_archive_corrupt_payload_length_and_descriptor(self):
        data = bytearray(zip_bytes([('a', b'hello tail')]))
        data[31] ^= 1
        name = self.put(data)
        with self.assertRaisesRegex(ValueError, 'CRC'):
            self.workspace.inspect_archive(name, member_path='a', length=1)
        original = bytearray(zip_bytes([('a', b'hello tail')]))
        original[22] ^= 1
        name = self.put(original)
        with self.assertRaises(ValueError):
            self.workspace.inspect_archive(name, member_path='a')
        class NonSeek(io.BytesIO):
            def seekable(self):
                return False
            def seek(self, *args):
                raise OSError()
        stream = NonSeek()
        with zipfile.ZipFile(stream, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
            archive.writestr('a', b'hello descriptor')
        name = self.put(stream.getvalue())
        self.assertEqual(base64.b64decode(self.workspace.inspect_archive(name, member_path='a')['data']), b'hello descriptor')
        damaged = bytearray(stream.getvalue())
        damaged[damaged.index(b'PK\x07\x08') + 4] ^= 1
        name = self.put(damaged)
        with self.assertRaises(ValueError):
            self.workspace.inspect_archive(name, member_path='a')

    def test_archive_bomb_entries_and_response_budget(self):
        for entries, compression in ([('a', b'0'*1000000)], zipfile.ZIP_DEFLATED), ([(str(i), b'') for i in range(4097)], 0):
            name = self.put(zip_bytes(entries, compression))
            with self.assertRaises(ValueError):
                self.workspace.inspect_archive(name, member_path='a')
        name = self.put(zip_bytes([('a', os.urandom(20000))]))
        result = self.workspace.inspect_archive(name, member_path='a', length=16384)
        self.assertLess(len(json.dumps(result).encode()), MAX_RESPONSE)
        for opts in ({'length': 16385}, {'offset': -1}, {'offset': True}, {'start': 1}):
            with self.assertRaises(ValueError):
                self.workspace.inspect_archive(name, member_path='a', **opts)

    def test_archive_unicode_directory_response_pagination(self):
        entries = [(str(i) + '漢'*1000, b'x') for i in range(100)]
        name = self.put(zip_bytes(entries))
        first = self.workspace.inspect_archive(name, limit=100)
        self.assertTrue(first['has_more'])
        self.assertLess(len(json.dumps(first, ensure_ascii=False).encode()), MAX_RESPONSE)
        second = self.workspace.inspect_archive(name, start=first['next_start'], limit=100)
        self.assertNotEqual(first['entries'][0]['name'], second['entries'][0]['name'])

    def test_archive_forged_matching_lengths_and_changed_snapshot(self):
        data = bytearray(zip_bytes([('a', b'hello expanded content')], zipfile.ZIP_DEFLATED))
        central = data.index(b'PK\x01\x02')
        struct.pack_into('<I', data, 22, 1)
        struct.pack_into('<I', data, central + 24, 1)
        name = self.put(data)
        with self.assertRaises(ValueError):
            self.workspace.inspect_archive(name, member_path='a')
        self.put(zip_bytes([('a', b'original')]))
        first = self.workspace.inspect_archive(name, member_path='a', length=1)
        self.put(zip_bytes([('a', b'changed')]))
        with self.assertRaisesRegex(ValueError, 'SOURCE_CHANGED'):
            self.workspace.inspect_archive(name, member_path='a', expected_sha256=first['source_sha256'])

    def test_archive_tar_special_and_truncated_gzip(self):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode='w') as archive:
            item = tarfile.TarInfo('a')
            item.type = tarfile.LNKTYPE
            item.linkname = 'target'
            archive.addfile(item)
        for raw in (output.getvalue(), gzip.compress(tar_bytes([('a', b'hello')]))[:-3]):
            name = self.put(raw)
            with self.assertRaises(ValueError):
                self.workspace.inspect_archive(name, member_path='a')

    def test_csv_tsv_types_multiline_pagination(self):
        for delimiter, fmt in ((',', 'CSV'), ('\t', 'TSV')):
            text = 'name' + delimiter + 'value\n"two\nlines"' + delimiter + '001\n'
            text += ''.join('row' + delimiter + str(i) + '\n' for i in range(2000))
            first = self.document(text.encode(), limit=2)
            self.assertEqual(first['format'], fmt)
            self.assertEqual(first['column_types'], ['string', 'string'])
            self.assertEqual(json.loads(first['units'][0]['text']), ['two\nlines', '001'])
            self.assertEqual(first['units'][0]['location']['line_end'], 3)
            second = self.workspace.read_document('sample.data', start=first['next_start'], limit=2,
                                                  expected_sha256=first['source_sha256'])
            self.assertEqual(second['units'][0]['location']['row'], 3)

    def test_csv_bad_tail_and_limits(self):
        for data in (b'a,b\n1,2\n3', b'a,b\n"unterminated', ('a\n' + 'x'*131073).encode(), (','.join('a' for _ in range(257))).encode()):
            self.rejected(data, format_hint='CSV', limit=1)

    def test_json_typed_paths_and_jsonl_lines(self):
        first = self.document(json.dumps({'a/b': [1, True, None, '長'*5000]}).encode(), limit=2)
        self.assertEqual(first['units'][0]['location']['pointer'], '/a~1b/0')
        self.assertEqual(first['units'][1]['location']['value_type'], 'boolean')
        result = self.document(b'{"id":1}\n\n{"id":2}\n', limit=1)
        self.assertEqual(result['format'], 'JSONL')
        next_page = self.workspace.read_document('sample.data', start=result['next_start'])
        self.assertEqual(next_page['units'][0]['location']['line'], 3)

    def test_json_corrupt_tail_duplicate_nonfinite_depth(self):
        for data, hint in ((b'{"a":1,"a":2}', 'JSON'), (b'{"x":NaN}', 'JSON'), (b'{"x":1e999}', 'JSON'),
                           (b'{"a":1}\n{broken}', 'JSONL'), (b'['*66+b'1'+b']'*66, 'JSON')):
            self.rejected(data, format_hint=hint, limit=1)

    def test_xml_locations_and_external_entity_rejection(self):
        result = self.document(b'<root><row id="1">value</row><row>tail</row></root>', limit=2)
        self.assertEqual(result['format'], 'XML')
        self.assertEqual(json.loads(result['units'][1]['text'])['attributes'], {'id': '1'})
        for data in (b'<!DOCTYPE a [<!ENTITY x SYSTEM "file:///private">]><a>&x;</a>', b'<a>good</a><bad>', b'<a>'*65+b'</a>'*65):
            self.rejected(data, limit=1)

    def test_yaml_safe_types_and_rejections(self):
        result = self.document(b'---\nname: test\nitems: [1, true, null]\n', limit=2)
        self.assertEqual(result['format'], 'YAML')
        for text in ('a: &a [1]\nb: *a', 'x: !!python/object/apply:os.system [calc]', 'a: 1\na: 2',
                     'x: .nan', '---\na: 1\n---\nb: 2', 'a: ['*66 + '0' + ']'*66):
            self.rejected(text.encode(), format_hint='YAML', limit=1)

    def test_file_info_new_structures_and_extension_spoofs(self):
        for data, expected in ((b'{"a":1}', 'JSON'), (b'{"a":1}\n{"a":2}', 'JSONL'),
                               (b'a,b\n1,2', 'CSV'), (b'a\tb\n1\t2', 'TSV'),
                               (b'<root/>', 'XML'), (b'---\na: 1', 'YAML'),
                               (b'From: a@b\nSubject: hi\n\nbody', 'EML'),
                               (b'WEBVTT\n\n00:01.000 --> 00:02.000\ncaption', 'VTT')):
            self.put(data, 'renamed.pdf')
            result = self.workspace.file_info('renamed.pdf')
            self.assertEqual(result['format'], expected)
            self.assertTrue(any(item.startswith('read_document') for item in result['capabilities']))
        self.put(b'not actually PDF', 'fake.pdf')
        self.assertEqual(self.workspace.file_info('fake.pdf')['format'], 'UTF-8')

    def test_toml_ini_no_interpolation_and_binary_spoof(self):
        result = self.document(b'[section]\nvalue = "${HOME}"\n', format_hint='INI')
        self.assertIn('${HOME}', ''.join(row['text'] for row in result['units']))
        if sys.version_info >= (3, 11):
            result = self.document(b'[section]\nvalue = 3\n', format_hint='TOML')
            self.assertEqual(result['units'][0]['location']['value_type'], 'integer')
        self.rejected(b'%PDF-1.7\n', format_hint='CSV')
        self.rejected(b'{"a":1}', table='sample')

    @unittest.skipUnless(sqlite_supported(), 'SQLite deserialize/setlimit unavailable')
    def test_sqlite_catalog_typed_rows_and_no_source_writes(self):
        data = database_bytes()
        catalog = self.document(data)
        self.assertEqual(catalog['mode'], 'table schema')
        self.assertEqual(catalog['units'][0]['location']['table'], 'sample')
        rows = self.workspace.read_document('sample.data', table='sample', limit=2)
        self.assertEqual(rows['columns'][0]['declared_type'], 'INTEGER')
        self.assertEqual(json.loads(rows['units'][0]['text'])[2]['type'], 'blob')
        next_rows = self.workspace.read_document('sample.data', table='sample', start=rows['next_start'], limit=2)
        self.assertEqual(next_rows['units'][0]['location']['row'], 2)
        self.assertEqual(list(self.root.iterdir()), [self.root/'sample.data'])
        self.assertEqual((self.root/'sample.data').read_bytes(), data)

    @unittest.skipUnless(sqlite_supported(), 'SQLite deserialize/setlimit unavailable')
    def test_sqlite_wal_corruption_sql_injection_views_generated(self):
        data = bytearray(database_bytes())
        data[18:20] = b'\x02\x02'
        self.rejected(data)
        self.rejected(database_bytes()[:200])
        self.rejected(database_bytes(), table='sample; ATTACH DATABASE x')
        data = database_bytes('CREATE TABLE generated(a INTEGER, b INTEGER GENERATED ALWAYS AS (a+1))')
        self.rejected(data, table='generated')
        db = sqlite3.connect(':memory:')
        try:
            db.executescript('CREATE TABLE t(a); CREATE VIEW evil AS SELECT load_extension(a) FROM t;')
            self.rejected(db.serialize(), table='evil')
        finally:
            db.close()

    def test_odf_all_types_and_repeats(self):
        examples = [('ODT', '<text><p>hello</p><p>world</p></text>'),
                    ('ODS', '<spreadsheet><table name="Sheet"><table-row number-rows-repeated="2"><table-cell value-type="float" value="3" formula="of:=1+2"><p>3</p></table-cell></table-row></table></spreadsheet>'),
                    ('ODP', '<presentation><page><p>slide marker</p></page></presentation>')]
        for fmt, content in examples:
            name = self.put(odf_bytes(fmt, content))
            self.assertEqual(self.workspace.file_info(name)['format'], fmt)
            result = self.workspace.read_document(name, limit=1)
            self.assertEqual(result['format'], fmt)
            self.assertTrue(result['units'])
        self.rejected(odf_bytes('ODS', '<table><table-row number-rows-repeated="1000000000"/></table>'))
        self.rejected(odf_bytes('ODT', '<p>broken'))

    def test_epub_order_and_external_traversal_entities(self):
        name = self.put(epub_bytes())
        self.assertEqual(self.workspace.file_info(name)['format'], 'EPUB')
        result = self.workspace.read_document(name)
        self.assertIn('chapter marker', result['units'][0]['text'])
        self.assertNotIn('NEVER EXECUTE', result['units'][0]['text'])
        for href in ('https://invalid.example/a', '../../a', '%2e%2e/a', '/private'):
            self.rejected(epub_bytes(href))
        self.rejected(epub_bytes(chapter='<!DOCTYPE html SYSTEM "https://invalid.example"><html><body>bad</body></html>'))

    def test_email_mime_html_attachments_and_mbox(self):
        message = EmailMessage()
        message['From'] = 'sender@example.invalid'
        message['Subject'] = '郵件 marker'
        message.set_content('body marker')
        message.add_alternative('<html><body><p>html marker</p><script>BAD</script><img src="https://invalid.example"/></body></html>', subtype='html')
        message.add_attachment(b'opaque bytes', maintype='application', subtype='octet-stream', filename='../never-extract')
        raw = message.as_bytes()
        result = self.document(raw)
        self.assertEqual(result['format'], 'EML')
        content = '\n'.join(row['text'] for row in result['units'])
        self.assertIn('body marker', content)
        self.assertIn('html marker', content)
        self.assertNotIn('BAD', content)
        self.assertIn(hashlib.sha256(b'opaque bytes').hexdigest(), content)
        mbox = (b'From sender Sat Oct  3 00:00:00 2026\n' + raw + b'\n') * 2
        result = self.document(mbox)
        self.assertEqual(result['messages'], 2)
        self.assertEqual(result['format'], 'MBOX')

    def test_email_malformed_base64_and_charset(self):
        for body in (b'From: a@b\nContent-Type: text/plain; charset=unknown-codec\n\nhello',
                     b'From: a@b\nContent-Transfer-Encoding: base64\n\n!!!!',
                     b'From: a@b\nContent-Type: multipart/mixed; boundary=x\n\nmissing boundary'):
            self.rejected(body, format_hint='EML')

    def test_images_ico_pnm_metadata_and_corrupt(self):
        for fmt, name in (('ICO', 'icon.ico'), ('PPM', 'image.ppm')):
            output = io.BytesIO()
            Image.new('RGB', (32, 32), (20, 40, 60)).save(output, format=fmt)
            self.put(output.getvalue(), name)
            result = self.workspace.read_image(name)
            self.assertEqual(result.content[1].mimeType, 'image/png')
            metadata = self.workspace.inspect_media(name)
            self.assertEqual(metadata['source_width'], 32)
            self.assertEqual(self.workspace.file_info(name)['format'], fmt)
            self.put(output.getvalue()[:10], name)
            with self.assertRaises(ValueError):
                self.workspace.inspect_media(name)
        self.rejected(b'P6\n999999999 999999999\n255\n', format_hint='CSV')

    def test_aiff_au_metadata_bounds(self):
        # IEEE extended 80-bit sample rate 8000, 16-bit mono silence.
        comm = struct.pack('>hIh', 1, 8, 16) + bytes.fromhex('400bfa00000000000000')
        chunks = b'COMM' + struct.pack('>I', len(comm)) + comm
        chunks += b'SSND' + struct.pack('>I', 24) + b'\0'*24
        aiff = b'FORM' + struct.pack('>I', len(chunks) + 4) + b'AIFF' + chunks
        au = b'.snd' + struct.pack('>5I', 24, 16, 3, 8000, 1) + b'\0'*16
        for data, fmt in ((aiff, 'AIFF'), (au, 'AU')):
            name = self.put(data)
            result = self.workspace.inspect_media(name)
            self.assertEqual(result['format'], fmt)
            self.assertEqual(result['tracks'][0]['sample_rate'], 8000)
        name = self.put(au[:-1])
        with self.assertRaises(ValueError):
            self.workspace.inspect_media(name)

    @unittest.skipUnless(features.check('avif'), 'Pinned Pillow build lacks AVIF')
    def test_avif_static_animated_spoofed_and_metadata(self):
        output = io.BytesIO()
        image = Image.new('RGB', (32, 24), (30, 60, 90))
        image.save(output, format='AVIF')
        name = self.put(output.getvalue(), 'photo.avif')
        self.assertEqual(self.workspace.file_info(name)['format'], 'AVIF')
        self.assertEqual(self.workspace.inspect_media(name)['source_height'], 24)
        self.assertEqual(self.workspace.read_image(name).content[1].mimeType, 'image/png')
        animated = io.BytesIO()
        image.save(animated, format='AVIF', save_all=True, append_images=[Image.new('RGB', (32, 24), 'red')], duration=100)
        self.put(animated.getvalue(), name)
        with self.assertRaises(ValueError):
            self.workspace.read_image(name)
        self.put(b'not an avif', name)
        with self.assertRaises(ValueError):
            self.workspace.read_image(name)

    def test_matroska_webm_metadata_and_truncated_elements(self):
        def element(tag, payload):
            identifier = tag.to_bytes((tag.bit_length()+7)//8, 'big')
            length = len(payload)
            width = 1 if length < 127 else 2
            return identifier + ((1 << (7*width)) | length).to_bytes(width, 'big') + payload
        info = element(0x1549a966, element(0x4489, struct.pack('>d', 2000)))
        video = element(0xe0, element(0xb0, b'\x02\x80') + element(0xba, b'\x01\x68'))
        track = element(0xae, element(0xd7, b'\x01') + element(0x83, b'\x01') + element(0x86, b'V_VP9') + video)
        for doctype in (b'matroska', b'webm'):
            header = element(0x1a45dfa3, element(0x4282, doctype))
            raw = header + element(0x18538067, info + element(0x1654ae6b, track))
            name = self.put(raw)
            result = self.workspace.inspect_media(name)
            self.assertEqual(result['duration'], 2)
            self.assertEqual(result['tracks'][0]['width'], 640)
            self.put(raw[:-1])
            with self.assertRaises(ValueError):
                self.workspace.inspect_media(name)
        too_many = header + element(0x18538067, info + element(0x1654ae6b, track*33))
        self.put(too_many)
        with self.assertRaises(ValueError):
            self.workspace.inspect_media(name)

    def test_response_budget_continuation_and_changed_snapshot(self):
        data = json.dumps(['漢'*6000 for _ in range(50)], ensure_ascii=False).encode()
        result = self.document(data, limit=100)
        self.assertTrue(result['response_limited'])
        self.assertLess(len(json.dumps(result, ensure_ascii=False).encode()), MAX_RESPONSE)
        second = self.workspace.read_document('sample.data', start=result['next_start'], expected_sha256=result['source_sha256'])
        self.assertEqual(second['units'][0]['index'], result['next_start'])
        self.put(b'{"changed":true}')
        with self.assertRaisesRegex(ValueError, 'SOURCE_CHANGED'):
            self.workspace.read_document('sample.data', expected_sha256=result['source_sha256'])

    def test_worker_audit_denies_file_network_process_sqlite_file_extension(self):
        script = '''import sys, sqlite3, socket, subprocess
from format_worker import deny_external_access
sys.addaudithook(deny_external_access)
checks = [lambda: open('forbidden','w'), lambda: socket.socket(), lambda: subprocess.run(['never']),
          lambda: sqlite3.connect('forbidden.db'), lambda: sqlite3.connect(':memory:').enable_load_extension(True)]
for check in checks:
    try:
        check()
    except ValueError as error:
        assert 'FORMAT_EXTERNAL_ACCESS' in str(error)
    else:
        raise AssertionError('external access permitted')
print('all blocked')
'''
        result = subprocess.run([sys.executable, '-B', '-c', script], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('all blocked', result.stdout)

    def test_worker_real_memory_ceiling(self):
        script = '''from format_worker import memory_limit
memory_limit()
try:
    data = bytearray(1024*1024*1024)
except MemoryError:
    print('allocation blocked')
else:
    raise AssertionError('memory ceiling not applied')
'''
        result = subprocess.run([sys.executable, '-B', '-c', script], capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('allocation blocked', result.stdout)

    def test_sidecar_subtitles_timing_continuation_and_corrupt_tail(self):
        srt = b'1\n00:00:01,000 --> 00:00:02,500\nhello\n\n2\n00:00:03,000 --> 00:00:04,000\nnext\n'
        first = self.document(srt, limit=1)
        self.assertEqual(first['format'], 'SRT')
        self.assertEqual(first['units'][0]['location']['end_seconds'], 2.5)
        self.assertEqual(self.workspace.read_document('sample.data', start=first['next_start'])['units'][0]['text'], 'next')
        vtt = b'WEBVTT\n\nchapter\n00:01.000 --> 00:02.000 align:start\nhello\n'
        self.assertEqual(self.document(vtt)['format'], 'VTT')
        self.rejected(srt + b'\n3\n00:99:00,000 --> bad\ntail', limit=1)

    def test_new_stdio_schema_registry_and_calls(self):
        self.put(b'{"value":[1,2,3]}', 'rows.json')
        self.put(zip_bytes([('a', b'marker')]), 'archive.zip')
        async def run():
            args = ['-B', str(Path('local_files_mcp.py').resolve()), '--root', str(self.root)]
            async with stdio_client(StdioServerParameters(command=sys.executable, args=args)) as (read, write):
                async with ClientSession(read, write) as session:
                    init = await session.initialize()
                    self.assertEqual(init.serverInfo.version, SERVICE_VERSION)
                    tools = (await session.list_tools()).tools
                    verify_contract(tools)
                    self.assertEqual(len(tools), 21)
                    for tool, arguments in [('read_document', {'path': 'rows.json', 'format_hint': 'JSON', 'limit': 1}),
                                            ('inspect_archive', {'path': 'archive.zip', 'member_path': 'a', 'length': 2})]:
                        result = await session.call_tool(tool, arguments)
                        self.assertFalse(result.isError, str(result.content))
                    for args in ({'path': 'archive.zip', 'member_path': '../a'}, {'path': 'archive.zip', 'member_path': 'a', 'length': '2'}):
                        self.assertTrue((await session.call_tool('inspect_archive', args)).isError)
                    diag = json.loads((await session.call_tool('server_diagnostics', {})).content[0].text)
                    info = json.loads((await session.call_tool('workspace_info', {})).content[0].text)
                    self.assertTrue(diag['consistent'])
                    self.assertEqual(diag['contract_version'], CONTRACT_VERSION)
                    self.assertEqual(diag['format_limits'], info['format_limits'])
                    self.assertTrue(info['format_limits']['archive_member_read'])
        asyncio.run(run())
