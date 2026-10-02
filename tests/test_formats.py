"""Real bounded format workers, hostile containers, guarded opens, and STDIO."""
import asyncio
import base64
import ctypes
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import wave
import zipfile

from PIL import Image
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from pypdf import PdfWriter
from pypdf.generic import DictionaryObject, NameObject, DecodedStreamObject

import format_reader as formats
from format_parsers import checked_zip, xml
from local_files_mcp import create_server
from operation_budget import Budget, OperationError, operation
from reader_settings import encode_reader_settings
from tool_contract import verify_contract, SERVICE_VERSION
from workspace_reader import WorkspaceReader


def office(kind, entries):
    part, mime = {'DOCX': ('word/document.xml', 'wordprocessingml.document'),
                  'XLSX': ('xl/workbook.xml', 'spreadsheetml.sheet'),
                  'PPTX': ('ppt/presentation.xml', 'presentationml.presentation')}[kind]
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, 'w') as archive:
        archive.writestr('[Content_Types].xml', '<Types><Override PartName="/' + part +
                         '" ContentType="application/vnd.openxmlformats-officedocument.' + mime + '.main+xml"/></Types>')
        for name, data in entries.items():
            archive.writestr(name, data)
    return buffer.getvalue()


def pdf_bytes(text='PDF marker', pages=2):
    writer = PdfWriter()
    for _ in range(pages):
        page = writer.add_blank_page(300, 300)
        font = DictionaryObject({NameObject('/Type'): NameObject('/Font'), NameObject('/Subtype'): NameObject('/Type1'),
                                 NameObject('/BaseFont'): NameObject('/Helvetica')})
        page[NameObject('/Resources')] = DictionaryObject({NameObject('/Font'): DictionaryObject({NameObject('/F1'): font})})
        stream = DecodedStreamObject()
        stream.set_data(('BT /F1 12 Tf 10 10 Td (' + text + ') Tj ET').encode())
        page[NameObject('/Contents')] = writer._add_object(stream)
    writer.add_js("app.launchURL('https://invalid.example/');")
    buffer = io.BytesIO()
    writer.write(buffer)
    return buffer.getvalue()


def wav_bytes():
    buffer = io.BytesIO()
    with wave.open(buffer, 'wb') as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(8000)
        out.writeframes(b'\0'*32000)
    return buffer.getvalue()


def mp4_bytes():
    def box(name, data):
        return struct.pack('>I4s', len(data)+8, name) + data
    mdhd = box(b'mdhd', b'\0'*12 + struct.pack('>II', 1000, 2000) + b'\0'*4)
    hdlr = box(b'hdlr', b'\0'*8+b'vide'+b'\0'*12)
    sample = box(b'avc1', b'\0'*24 + struct.pack('>HH', 640, 360) + b'\0'*50)
    stsd = box(b'stsd', b'\0'*4+struct.pack('>I', 1)+sample)
    stts = box(b'stts', b'\0'*4+struct.pack('>III', 1, 50, 40))
    trak = box(b'trak', box(b'mdia', mdhd+hdlr+box(b'minf', box(b'stbl', stsd+stts))))
    return box(b'ftyp', b'isom\0\0\0\0isom')+box(b'moov', trak)+box(b'mdat', b'\0'*16)


class FormatTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.workspace = WorkspaceReader([dict(id='main', path=str(self.root))], 'main',
                                         {'max_file_bytes': 32*1024*1024})
        self.reader = self.workspace.reader(None)

    def put(self, name, data):
        path = self.root / name
        path.write_bytes(data)
        return path

    def test_discovery_is_not_text_filter_and_stays_lightweight(self):
        for name in ('text.txt', 'image.png', 'doc.pdf', 'sheet.xlsx', 'video.mp4', 'archive.zip', 'app.exe', 'unknown.xyz'):
            self.put(name, b'\0\xff')
        self.put('.hidden.bin', b'x')
        self.put('secrets.json', b'x')
        with patch('format_reader.parse_snapshot', side_effect=AssertionError('listing parsed content')):
            self.assertEqual(len(self.workspace.list_files()['files']), 8)
            self.assertEqual(len(self.workspace.list_directory()['entries']), 8)
            self.assertEqual(len(self.workspace.find_files(['.'])['results'][0]['matches']), 8)
        for path in self.workspace.list_files()['files']:
            info = self.workspace.file_info(path)
            self.assertEqual(info['size'], 2)
            self.assertIn('file_info', info['capabilities'])
        with self.assertRaises(ValueError):
            self.workspace.read_file('doc.pdf')
        with self.assertRaises(ValueError):
            self.workspace.read_file('text.txt')

    def test_metadata_signature_times_attributes_and_large_file(self):
        self.put('renamed.txt', b'%PDF-1.7\n')
        result = self.workspace.file_info('renamed.txt')
        self.assertEqual(result['format'], 'PDF')
        self.assertEqual(result['mime_type'], 'application/pdf')
        self.assertIn('modified_time', result)
        self.assertIn('windows', result['attributes'])
        self.assertFalse(any(c.startswith('read_file ') for c in result['capabilities']))
        with self.assertRaises(ValueError):
            self.workspace.read_file('renamed.txt')
        self.assertEqual(self.workspace.search_text('PDF')['matches'], [])
        self.reader.settings['max_file_bytes'] = 8
        result = self.workspace.file_info('renamed.txt')
        self.assertEqual(result['capabilities'], ['file_info'])

    def test_binary_hash_ranges_compare_status(self):
        data = bytes(range(256))*20
        self.put('one.bin', data)
        self.put('two.bin', data)
        self.assertEqual(self.workspace.hash_files(['one.bin'])['files'][0]['sha256'], hashlib.sha256(data).hexdigest())
        result = self.workspace.read_binary('one.bin', 200, 100)
        self.assertEqual(base64.b64decode(result['data']), data[200:300])
        self.assertEqual(self.workspace.read_binary('one.bin', 9999)['returned_bytes'], 0)
        self.assertEqual(self.workspace.compare_paths('one.bin', 'two.bin')['counts']['same'], 1)
        baseline = self.workspace.project_status()
        self.put('one.bin', b'changed\0')
        result = self.workspace.project_status(baseline_id=baseline['baseline_id'])
        self.assertEqual(result['changes']['modified'][0]['path'], 'one.bin')
        for offset, length in [(-1, 1), (0, 16385), (True, 1), (0, False)]:
            with self.assertRaises(ValueError):
                self.workspace.read_binary('one.bin', offset, length)

    def test_docx_segments_resume_and_source_unchanged(self):
        data = office('DOCX', {'word/document.xml': '<document><body><p><r><t>' + 'x'*5001 +
                              '</t></r></p><p><r><t>next</t></r></p></body></document>'})
        path = self.put('renamed.data', data)
        before = path.stat().st_mtime_ns
        self.assertEqual(self.workspace.file_info(path.name)['format'], 'DOCX')
        first = self.workspace.read_document(path.name, limit=2)
        self.assertEqual(first['format'], 'DOCX')
        self.assertEqual(first['next_start'], 2)
        second = self.workspace.read_document(path.name, start=2, limit=2)
        self.assertFalse(second['has_more'])
        self.assertEqual(''.join(u['text'] for u in first['units'] + second['units']), 'x'*5001+'next')
        self.assertEqual(path.read_bytes(), data)
        self.assertEqual(path.stat().st_mtime_ns, before)

    def test_xlsx_values_formulas_and_external_reference_not_followed(self):
        self.put('book.xlsx', office('XLSX', {
            'xl/workbook.xml': '<workbook xmlns:r="rel"><sheets><sheet name="Data" r:id="r1"/></sheets></workbook>',
            'xl/_rels/workbook.xml.rels': '<Relationships><Relationship Id="r1" Target="worksheets/sheet1.xml"/>'
                '<Relationship Id="evil" TargetMode="External" Target="file:///secrets.json"/></Relationships>',
            'xl/sharedStrings.xml': '<sst><si><t>Hello</t></si></sst>',
            'xl/worksheets/sheet1.xml': '<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c>'
                '<c r="B1"><f>1+2</f><v>3</v></c></row></sheetData></worksheet>'}))
        result = self.workspace.read_document('book.xlsx')
        row = json.loads(result['units'][0]['text'])
        self.assertEqual(row[0]['value'], 'Hello')
        self.assertEqual(row[1]['formula'], '1+2')
        self.assertEqual(row[1]['value'], '3')

    def test_pptx_preserves_presentation_order(self):
        self.put('deck.pptx', office('PPTX', {
            'ppt/presentation.xml': '<presentation xmlns:r="rel"><sldIdLst><sldId r:id="r2"/><sldId r:id="r1"/></sldIdLst></presentation>',
            'ppt/_rels/presentation.xml.rels': '<Relationships><Relationship Id="r1" Target="slides/slide1.xml"/>'
                '<Relationship Id="r2" Target="slides/slide2.xml"/></Relationships>',
            'ppt/slides/slide1.xml': '<sld><p><r><t>second</t></r></p></sld>',
            'ppt/slides/slide2.xml': '<sld><p><r><t>first</t></r></p></sld>'}))
        result = self.workspace.read_document('deck.pptx')
        self.assertEqual([u['text'] for u in result['units']], ['first', 'second'])

    def test_pdf_text_and_active_content_ignored(self):
        self.put('report.pdf', pdf_bytes())
        result = self.workspace.read_document('report.pdf', limit=1)
        self.assertEqual(result['pages'], 2)
        self.assertIn('PDF marker', result['units'][0]['text'])
        self.assertEqual(result['next_start'], 1)
        self.assertEqual(self.workspace.read_document('report.pdf', start=1)['units'][0]['location'], {'page': 2})

    def test_document_corrupt_spoof_encrypted_and_xml_entities(self):
        self.put('fake.pdf', b'not pdf')
        self.put('broken.pdf', b'%PDF-1.7\nwrong')
        self.put('fake.docx', office('DOCX', {'word/not-main.xml': '<doc/>'}))
        self.put('entity.docx', office('DOCX', {'word/document.xml': '<!DOCTYPE x [<!ENTITY x SYSTEM "file:///secret">]><x>&x;</x>'}))
        writer = PdfWriter()
        writer.add_blank_page(10, 10)
        writer.encrypt('secret')
        out = io.BytesIO()
        writer.write(out)
        self.put('encrypted.pdf', out.getvalue())
        for name in ('fake.pdf', 'broken.pdf', 'fake.docx', 'entity.docx', 'encrypted.pdf'):
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.workspace.read_document(name)

    def test_xml_depth_nodes_and_document_capacity(self):
        with self.assertRaisesRegex(ValueError, 'XML_LIMIT'):
            xml(('<x>'*65+'</x>'*65).encode())
        with self.assertRaisesRegex(ValueError, 'XML_LIMIT'):
            xml(('<x>'+'<a/>'*200000+'</x>').encode())
        self.put('large.pdf', b'%PDF-'+b'x'*1000)
        self.reader.settings['max_file_bytes'] = 100
        with self.assertRaises(ValueError):
            self.workspace.read_document('large.pdf')
        self.assertEqual(self.workspace.file_info('large.pdf')['size'], 1005)

    def test_giant_pdf_and_decompression_limit(self):
        self.put('pages.pdf', pdf_bytes(pages=2001))
        with self.assertRaises(ValueError):
            self.workspace.read_document('pages.pdf')
        writer = PdfWriter()
        page = writer.add_blank_page(10, 10)
        stream = DecodedStreamObject()
        stream.set_data(b' '*(8*1024*1024+1))
        page[NameObject('/Contents')] = writer._add_object(stream.flate_encode())
        output = io.BytesIO()
        writer.write(output)
        self.put('compressed.pdf', output.getvalue())
        with self.assertRaises(ValueError):
            self.workspace.read_document('compressed.pdf')

    def test_worker_resource_limit_and_external_access_enforced(self):
        script = '''import sys, socket, subprocess
from format_worker import memory_limit, deny_external_access
memory_limit()
try:
    bytearray(768*1024*1024)
except MemoryError:
    print('MEMORY_BLOCKED')
else:
    raise AssertionError('memory limit not applied')
sys.addaudithook(deny_external_access)
for action in (lambda: open('never-opened', 'w'), lambda: socket.socket(),
               lambda: subprocess.Popen([sys.executable, '-V'])):
    try:
        action()
    except ValueError:
        print('ACCESS_BLOCKED')
    else:
        raise AssertionError('access guard missing')
'''
        result = subprocess.run([sys.executable, '-B', '-c', script], capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr.decode(errors='replace'))
        self.assertIn(b'MEMORY_BLOCKED', result.stdout)
        self.assertEqual(result.stdout.count(b'ACCESS_BLOCKED'), 3)

    def test_parser_launch_is_fixed_and_credentials_not_inherited(self):
        self.put('audio.wav', wav_bytes())
        original = subprocess.Popen
        with patch.dict(os.environ, {'CONTROL_PLANE_API_KEY': 'fixture-only-not-a-key', 'PYTHONPATH': 'untrusted'}):
            with patch('format_reader.subprocess.Popen', wraps=original) as spawned:
                self.workspace.inspect_media('audio.wav')
            args, kwargs = spawned.call_args
            self.assertEqual(args[0][:3], [sys.executable, '-I', '-B'])
            self.assertEqual(Path(args[0][3]).name, 'format_worker.py')
            self.assertNotIn('shell', kwargs)
            self.assertNotIn('CONTROL_PLANE_API_KEY', kwargs['env'])
            self.assertNotIn('PYTHONPATH', kwargs['env'])

    def test_new_tools_reject_symlink(self):
        path = self.put('data.bin', b'123')
        try:
            (self.root/'link.bin').symlink_to(path)
        except OSError:
            self.skipTest('Symlink privilege unavailable')
        for call in (self.workspace.file_info, self.workspace.read_binary, self.workspace.read_document,
                     self.workspace.inspect_media, self.workspace.inspect_archive):
            with self.assertRaises(ValueError):
                call('link.bin')

    def zip_data(self, items, compression=zipfile.ZIP_STORED):
        out = io.BytesIO()
        with zipfile.ZipFile(out, 'w', compression=compression) as archive:
            for name, content in items:
                archive.writestr(name, content)
        return out.getvalue()

    def test_archive_pagination_nested_opaque_and_exclusions(self):
        nested = self.zip_data([('../evil', b'bad')])
        self.put('bundle.zip', self.zip_data([('a.txt', b'a'), ('b.bin', b'\0'), ('nested.zip', nested), ('secrets.json', b'private')]))
        first = self.workspace.inspect_archive('bundle.zip', limit=2)
        self.assertTrue(first['has_more'])
        self.assertEqual(first['skipped_entries'], 1)
        second = self.workspace.inspect_archive('bundle.zip', start=2)
        self.assertEqual(second['entries'][0]['name'], 'nested.zip')
        self.assertEqual(sorted(p.name for p in self.root.iterdir()), ['bundle.zip'])

    def test_archive_slip_link_duplicate_bomb_and_bad_headers(self):
        link = zipfile.ZipInfo('link')
        link.create_system = 3
        link.external_attr = 0o120777 << 16
        bad = [self.zip_data([(name, b'x')]) for name in ('../escape', '/abs', 'C:/file', 'NUL', 'trailing.')]
        # ZipInfo on Windows normalizes backslashes; mutate both raw headers.
        bad.append(self.zip_data([('a/b', b'x')]).replace(b'a/b', b'a\\b'))
        bad += [self.zip_data([(link, b'target')]), self.zip_data([('A', b'1'), ('a', b'2')]),
                self.zip_data([('huge', b'x'*100000)], zipfile.ZIP_DEFLATED), b'PK\x03\x04broken']
        for data in bad:
            with self.subTest(header=data[:10]), self.assertRaises(ValueError):
                checked_zip(data)
        self.put('bomb.zip', bad[-2])
        with self.assertRaisesRegex(ValueError, 'ARCHIVE_LIMIT'):
            self.workspace.inspect_archive('bomb.zip')

    def test_archive_count_member_and_response_limits(self):
        data = self.zip_data([(str(i), b'') for i in range(4097)])
        with self.assertRaisesRegex(ValueError, 'ARCHIVE_LIMIT'):
            checked_zip(data)
        data = bytearray(self.zip_data([('item', b'x')]))
        central = data.index(b'PK\x01\x02')
        struct.pack_into('<I', data, central+24, 8*1024*1024+1)
        with self.assertRaisesRegex(ValueError, 'ARCHIVE_LIMIT'):
            checked_zip(bytes(data))
        self.put('response.docx', office('DOCX', {'word/document.xml': '<document><p><t>'+'界'*100000+'</t></p></document>'}))
        result = self.workspace.read_document('response.docx', limit=100)
        self.assertTrue(result['response_limited'])
        self.assertLess(len(json.dumps(result, ensure_ascii=False).encode()), formats.MAX_RESPONSE)
        self.assertIsNotNone(result['next_start'])

    def test_tar_and_targz_and_link_rejected(self):
        for mode, name in [('w', 'bundle.tar'), ('w:gz', 'bundle.tgz')]:
            out = io.BytesIO()
            with tarfile.open(fileobj=out, mode=mode) as archive:
                item = tarfile.TarInfo('normal.bin')
                item.size = 3
                archive.addfile(item, io.BytesIO(b'abc'))
            self.put(name, out.getvalue())
            self.assertEqual(self.workspace.inspect_archive(name)['entries'][0]['size'], 3)
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode='w') as archive:
            item = tarfile.TarInfo('link')
            item.type = tarfile.SYMTYPE
            item.linkname = '../escape'
            archive.addfile(item)
        self.put('link.tar', out.getvalue())
        with self.assertRaisesRegex(ValueError, 'ARCHIVE_LINK'):
            self.workspace.inspect_archive('link.tar')

    def test_audio_and_video_metadata(self):
        self.put('audio.wav', wav_bytes())
        info = self.workspace.inspect_media('audio.wav')['tracks'][0]
        self.assertEqual(info['sample_rate'], 8000)
        self.assertEqual(info['channels'], 2)
        self.assertEqual(info['duration'], 1)
        self.put('video.renamed', mp4_bytes())
        track = self.workspace.inspect_media('video.renamed')['tracks'][0]
        self.assertEqual((track['width'], track['height'], track['fps'], track['duration']), (640, 360, 25, 2))
        self.assertEqual(track['codecs'], ['avc1'])
        self.put('audio.mp3', (b'\xff\xfb\x90\x64'+b'\0'*413)*20)
        self.assertGreater(self.workspace.inspect_media('audio.mp3')['tracks'][0]['duration'], 0)

    def test_media_spoof_and_corrupt_container(self):
        for name, data in [('fake.mp4', b'hello'), ('bad.mp4', mp4_bytes()[:-1]), ('fake.wav', b'RIFF\0\0\0\0WAVE')]:
            self.put(name, data)
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.workspace.inspect_media(name)

    def test_flac_opus_tags_and_avi_metadata(self):
        # Metadata-only synthetic containers; this test makes no playback claim.
        packed = (8000 << 44) | (1 << 41) | (15 << 36) | 8000
        streaminfo = struct.pack('>HH', 16, 16) + b'\0'*6 + packed.to_bytes(8, 'big') + b'\0'*16
        self.put('audio.flac', b'fLaC\x80\0\0\x22' + streaminfo)
        info = self.workspace.inspect_media('audio.flac')['tracks'][0]
        self.assertEqual((info['sample_rate'], info['channels'], info['duration']), (8000, 2, 1))
        from mutagen.ogg import OggPage
        packets = [b'OpusHead'+struct.pack('<BBHIhB', 1, 2, 0, 48000, 0, 0),
                   b'OpusTags'+struct.pack('<I', 4)+b'test'+struct.pack('<II', 1, 12)+b'TITLE=marker', b'\xf8\xff\xfe']
        pages = []
        for i, packet in enumerate(packets):
            page = OggPage()
            page.serial = 1
            page.sequence = i
            page.first, page.last = i == 0, i == 2
            page.position = 48000 if i == 2 else 0
            page.packets = [packet]
            pages.append(page.write())
        self.put('audio.opus', b''.join(pages))
        result = self.workspace.inspect_media('audio.opus')
        self.assertEqual(result['tracks'][0]['sample_rate'], 48000)
        self.assertEqual(result['tags']['title'], ['marker'])
        def chunk(kind, data):
            return kind + struct.pack('<I', len(data)) + data + (b'\0' if len(data) & 1 else b'')
        main = [40000, 1000, 0, 0, 50, 0, 1, 0, 640, 360, 0, 0, 0, 0]
        stream = bytearray(56)
        stream[:8] = b'vidsMJPG'
        struct.pack_into('<4I', stream, 20, 1, 25, 0, 50)
        hdrl = b'hdrl'+chunk(b'avih', struct.pack('<14I', *main))+chunk(b'LIST', b'strl'+chunk(b'strh', stream))
        content = b'AVI '+chunk(b'LIST', hdrl)
        self.put('video.avi', b'RIFF'+struct.pack('<I', len(content))+content)
        info = self.workspace.inspect_media('video.avi')
        self.assertEqual((info['width'], info['height'], info['fps'], info['duration']), (640, 360, 25, 2))

    def test_new_static_images_and_multiframe_rejected(self):
        for name, fmt in [('a.gif', 'GIF'), ('b.bmp', 'BMP'), ('c.tiff', 'TIFF')]:
            with Image.new('RGB', (40, 30), 'red') as im:
                im.save(self.root / name, format=fmt)
            result = self.workspace.read_image(name)
            self.assertEqual(result.content[1].mimeType, 'image/png')
        with Image.new('RGB', (20, 20), 'red') as a, Image.new('RGB', (20, 20), 'blue') as b:
            for name, fmt in [('multi.gif', 'GIF'), ('multi.tiff', 'TIFF')]:
                a.save(self.root/name, format=fmt, save_all=True, append_images=[b])
                with self.assertRaises(ValueError):
                    self.workspace.read_image(name)

    def test_all_new_tools_keep_path_hidden_link_and_policy_guards(self):
        self.put('normal.bin', b'123')
        self.put('.hidden.bin', b'123')
        self.put('secrets.json', b'123')
        os.link(self.root/'normal.bin', self.root/'hard.bin')
        calls = [self.workspace.file_info, self.workspace.read_document, self.workspace.inspect_media,
                 self.workspace.inspect_archive, self.workspace.read_binary]
        for call in calls:
            for path in ('../outside', '.hidden.bin', 'secrets.json', 'normal.bin', 'hard.bin', 'normal.bin:stream'):
                with self.subTest(tool=call.__name__, path=path), self.assertRaises(ValueError):
                    call(path)

    def test_windows_hidden_and_reparse(self):
        path = self.put('hidden.bin', b'123')
        if os.name == 'nt':
            old = ctypes.windll.kernel32.GetFileAttributesW(str(path))
            ctypes.windll.kernel32.SetFileAttributesW(str(path), old | 2)
            try:
                with self.assertRaises(ValueError):
                    self.workspace.file_info(path.name)
                self.assertEqual(self.workspace.list_files()['files'], [])
            finally:
                ctypes.windll.kernel32.SetFileAttributesW(str(path), old)
        original = __import__('local_files_mcp').linked
        with patch('local_files_mcp.linked', side_effect=lambda p, info=None: p == path or original(p, info)):
            with self.assertRaises(ValueError):
                self.workspace.read_binary(path.name)

    def test_toctou_during_new_reads(self):
        path = self.put('item.bin', b'123')
        original = os.fstat
        calls = 0
        def changed(fd):
            nonlocal calls
            calls += 1
            info = original(fd)
            values = {key: getattr(info, key) for key in dir(info) if key.startswith('st_')}
            if calls == 3:
                values['st_size'] += 1
            return SimpleNamespace(**values)
        for call in (self.workspace.file_info, self.workspace.read_binary, self.workspace.read_document,
                     self.workspace.inspect_media, self.workspace.inspect_archive):
            calls = 0
            with patch('local_files_mcp.os.fstat', side_effect=changed):
                with patch('format_reader.parse_snapshot', side_effect=AssertionError('must not parse changed data')):
                    with self.subTest(tool=call.__name__), self.assertRaises(ValueError):
                        call(path.name)

    def test_real_worker_timeout_and_cancel(self):
        with patch('format_reader.PARSER_TIMEOUT', .01):
            started = time.monotonic()
            with self.assertRaisesRegex(ValueError, 'FORMAT_TIMEOUT'):
                formats.parse_snapshot('document', pdf_bytes(), start=0, limit=1)
            self.assertLess(time.monotonic()-started, 3)
        budget = Budget()
        with operation(budget):
            budget.cancel.set()
            with self.assertRaises(OperationError):
                formats.parse_snapshot('media', wav_bytes())

    def test_stdio_contract_new_tools_and_strict_inputs(self):
        self.put('report.pdf', pdf_bytes())
        self.put('audio.wav', wav_bytes())
        self.put('binary.bin', b'\0\xff')
        self.put('archive.zip', self.zip_data([('entry.txt', b'hello')]))
        async def run():
            args = ['-B', str(Path('local_files_mcp.py').resolve()), '--root', str(self.root)]
            async with stdio_client(StdioServerParameters(command=sys.executable, args=args)) as (read, write):
                async with ClientSession(read, write) as session:
                    init = await session.initialize()
                    self.assertEqual(init.serverInfo.version, SERVICE_VERSION)
                    tools = (await session.list_tools()).tools
                    verify_contract(tools)
                    self.assertEqual(len(tools), 21)
                    for name, args in [('file_info', {'path': 'binary.bin'}), ('read_binary', {'path': 'binary.bin'}),
                                       ('read_document', {'path': 'report.pdf'}), ('inspect_media', {'path': 'audio.wav'}),
                                       ('inspect_archive', {'path': 'archive.zip'}), ('hash_files', {'paths': ['binary.bin']})]:
                        result = await session.call_tool(name, args)
                        self.assertFalse(result.isError, str(result.content))
                    for name, args in [('read_binary', {'path': 'binary.bin', 'length': 16385}),
                                       ('read_document', {'path': 'report.pdf', 'start': '0'}),
                                       ('file_info', {'path': 'binary.bin', 'execute': True})]:
                        self.assertTrue((await session.call_tool(name, args)).isError)
                    diag = json.loads((await session.call_tool('server_diagnostics', {})).content[0].text)
                    self.assertTrue(diag['consistent'])
                    self.assertEqual(diag['format_limits']['parser_timeout_seconds'], 10)
        asyncio.run(run())
