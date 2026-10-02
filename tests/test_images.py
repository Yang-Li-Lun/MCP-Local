"""Native image decoding, source boundaries, transport budgets and real STDIO."""
import asyncio
import base64
import ctypes
import hashlib
import io
import json
import os
from pathlib import Path
import random
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from PIL import Image, PngImagePlugin
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client
from mcp.types import CallToolResult, ImageContent, TextContent
from image_reader import MAX_IMAGE_BYTES, MAX_IMAGE_RESPONSE_BYTES
from local_files_mcp import create_server
from operation_budget import ensure_output_limit, OperationError, Budget, operation
from reader_settings import encode_reader_settings
from tool_contract import verify_contract, SERVICE_VERSION
from workspace_reader import WorkspaceReader


class ImageTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.workspace = WorkspaceReader([dict(id='main', name='main', path=str(self.root))],
                                         'main', {'max_file_bytes': 32 * 1024 * 1024})
        self.reader = self.workspace.reader(None)

    def fixture(self, name='image.png', fmt='PNG', size=(120, 80), noise=False):
        if noise:
            im = Image.frombytes('RGB', size, random.Random(523).randbytes(size[0]*size[1]*3))
        else:
            im = Image.new('RGB', size, (23, 91, 175))
        with im:
            im.save(self.root / name, format=fmt)
        return self.root / name

    def decoded(self, result):
        self.assertIsInstance(result, CallToolResult)
        self.assertIsNone(result.structuredContent)
        images = [c for c in result.content if isinstance(c, ImageContent)]
        self.assertEqual(len(images), 1)
        self.assertEqual(images[0].mimeType, 'image/png')
        binary = base64.b64decode(images[0].data, validate=True)
        self.assertLessEqual(len(binary), MAX_IMAGE_BYTES)
        self.assertLessEqual(len(result.model_dump_json().encode()), MAX_IMAGE_RESPONSE_BYTES)
        image = Image.open(io.BytesIO(binary))
        image.load()
        self.addCleanup(image.close)
        return image

    def test_supported_formats_and_source_unchanged(self):
        for name, fmt in [('a.png', 'PNG'), ('b.jpg', 'JPEG'), ('c.jpeg', 'JPEG'), ('d.webp', 'WEBP'), ('E.PNG', 'PNG')]:
            path = self.fixture(name, fmt)
            before = (hashlib.sha256(path.read_bytes()).digest(), path.stat().st_mtime_ns)
            self.assertEqual(self.decoded(self.workspace.read_image(name)).size, (120, 80))
            self.assertEqual(before, (hashlib.sha256(path.read_bytes()).digest(), path.stat().st_mtime_ns))

    def test_corrupt_and_spoofed_formats(self):
        for fmt, name in [('JPEG', 'spoof.png'), ('PNG', 'spoof.jpg'), ('WEBP', 'spoof.jpeg'), ('GIF', 'spoof.webp')]:
            self.fixture(name, fmt)
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.workspace.read_image(name)
        for fmt, name in [('PNG', 'bad.png'), ('JPEG', 'bad.jpg'), ('WEBP', 'bad.webp')]:
            path = self.fixture(name, fmt)
            path.write_bytes(path.read_bytes()[:len(path.read_bytes())//2])
            with self.subTest(name=name), self.assertRaises(ValueError):
                self.workspace.read_image(name)
        (self.root/'text.png').write_text('not an image')
        with self.assertRaises(ValueError):
            self.workspace.read_image('text.png')

    def test_png_crc_and_jpeg_truncated_tail(self):
        png = self.fixture()
        data = bytearray(png.read_bytes()); data[-5] ^= 0xFF; png.write_bytes(data)
        with self.assertRaises(ValueError):
            self.workspace.read_image('image.png')
        jpeg = self.fixture('tail.jpg', 'JPEG')
        jpeg.write_bytes(jpeg.read_bytes()[:-2])
        with self.assertRaises(ValueError):
            self.workspace.read_image('tail.jpg')

    def test_source_capacity_and_existing_root_limit(self):
        path = self.fixture()
        self.reader.settings['max_file_bytes'] = path.stat().st_size - 1
        with self.assertRaises(ValueError):
            self.workspace.read_image('image.png')
        self.reader.settings['max_file_bytes'] = 32 * 1024 * 1024
        with path.open('wb') as handle:
            handle.truncate(16 * 1024 * 1024 + 1)
        with self.assertRaises(ValueError):
            self.workspace.read_image('image.png')

    def test_pixel_and_dimension_limits_before_decode(self):
        for size in [(16385, 1), (5001, 5000)]:
            self.fixture(size=size)
            with patch('PIL.Image.Image.load', side_effect=AssertionError('must reject before decoding')):
                with self.assertRaises(ValueError):
                    self.workspace.read_image('image.png')

    def test_resize_keeps_aspect_ratio(self):
        self.fixture(size=(4096, 1024))
        self.assertEqual(self.decoded(self.workspace.read_image('image.png')).size, (2048, 512))

    def test_large_native_response_exceeds_old_json_budget(self):
        self.fixture(size=(900, 900), noise=True)
        result = self.workspace.read_image('image.png')
        self.assertGreater(len(result.content[1].data), 2 * 1024 * 1024)
        self.decoded(result)
        ensure_output_limit(result)
        with self.assertRaises(OperationError):
            ensure_output_limit({'text': 'x' * (2 * 1024 * 1024)})

    def test_encoded_budget_triggers_resize(self):
        self.fixture(size=(1400, 1400), noise=True)
        decoded = self.decoded(self.workspace.read_image('image.png'))
        self.assertLess(decoded.width, 1400)
        self.assertEqual(decoded.width, decoded.height)

    def test_native_budget_rejects_excess_and_structured_base64(self):
        self.fixture()
        result = self.workspace.read_image('image.png')
        result.content[1].data = 'A' * (4 * 1024 * 1024 + 1)
        with self.assertRaises(OperationError):
            ensure_output_limit(result)
        result = self.workspace.read_image('image.png')
        result.structuredContent = {'base64': 'abcd'}
        with self.assertRaises(OperationError):
            ensure_output_limit(result)

    def test_metadata_stripped_alpha_preserved_and_orientation(self):
        info = PngImagePlugin.PngInfo(); info.add_text('secret', 'DO_NOT_RETURN')
        with Image.new('RGBA', (80, 60), (1, 2, 3, 40)) as image:
            image.save(self.root/'alpha.png', pnginfo=info)
        decoded = self.decoded(self.workspace.read_image('alpha.png'))
        self.assertEqual(decoded.getpixel((0, 0)), (1, 2, 3, 40))
        self.assertEqual(decoded.info, {})
        with Image.new('RGB', (120, 80)) as image:
            exif = Image.Exif(); exif[274] = 6
            image.save(self.root/'rotated.jpg', exif=exif)
        self.assertEqual(self.decoded(self.workspace.read_image('rotated.jpg')).size, (80, 120))

    def test_animation_rejected(self):
        for name, fmt in [('animated.png', 'PNG'), ('animated.webp', 'WEBP')]:
            with Image.new('RGB', (20, 20), 'red') as a, Image.new('RGB', (20, 20), 'blue') as b:
                a.save(self.root/name, format=fmt, save_all=True, append_images=[b], duration=100)
            with self.assertRaises(ValueError):
                self.workspace.read_image(name)

    def test_outside_hidden_exclusions_and_state_directory(self):
        self.fixture()
        for path in ['../image.png', str(self.root/'image.png'), 'image.png:stream', '.hidden.png', 'build/image.png', 'blocked/image.png']:
            with self.subTest(path=path), self.assertRaises((ValueError, OSError)):
                self.workspace.read_image(path)
        self.fixture('.hidden.png')
        (self.root/'build').mkdir(); self.fixture('build/image.png')
        (self.root/'blocked').mkdir(); self.fixture('blocked/image.png')
        self.reader.settings['excluded_names'].append('blocked')
        for path in ['.hidden.png', 'build/image.png', 'blocked/image.png']:
            with self.assertRaises(ValueError):
                self.workspace.read_image(path)
        (self.root/'state').mkdir(); self.fixture('state/image.png')
        with patch('security_policy.STATE_DIR', self.root/'state'):
            with self.assertRaises(ValueError):
                self.workspace.read_image('state/image.png')

    @unittest.skipUnless(os.name == 'nt', 'Windows Hidden attributes')
    def test_windows_hidden_file_and_parent(self):
        self.fixture()
        (self.root/'child').mkdir(); self.fixture('child/nested.png')
        for target, relative in [(self.root/'image.png', 'image.png'), (self.root/'child', 'child/nested.png')]:
            original = ctypes.windll.kernel32.GetFileAttributesW(str(target))
            self.assertTrue(ctypes.windll.kernel32.SetFileAttributesW(str(target), original | 2))
            try:
                with self.assertRaises(ValueError):
                    self.workspace.read_image(relative)
            finally:
                ctypes.windll.kernel32.SetFileAttributesW(str(target), original)

    def test_hardlink_and_reparse_rejected(self):
        path = self.fixture()
        os.link(path, self.root/'hard.png')
        with self.assertRaises(ValueError):
            self.workspace.read_image('hard.png')
        (self.root/'hard.png').unlink()
        import local_files_mcp
        original = local_files_mcp.linked
        with patch('local_files_mcp.linked', side_effect=lambda p, info=None: p == path or original(p, info)):
            with self.assertRaises(ValueError):
                self.workspace.read_image('image.png')

    def test_symlink_rejected(self):
        path = self.fixture()
        try:
            (self.root/'sym.png').symlink_to(path)
        except OSError as exc:
            self.skipTest('Symlink privilege unavailable: ' + str(exc.winerror))
        with self.assertRaises(ValueError):
            self.workspace.read_image('sym.png')

    def test_file_identity_and_mutation_rejected(self):
        path = self.fixture()
        real = os.fstat
        for field, delta, on_call in [('st_ino', 1, 1), ('st_nlink', 1, 1), ('st_mtime_ns', 1, 2), ('st_size', 1, 2)]:
            count = 0
            def changed(fd):
                nonlocal count
                count += 1
                s = real(fd)
                values = {n: getattr(s, n) for n in dir(s) if n.startswith('st_')}
                if count == on_call:
                    values[field] += delta
                return SimpleNamespace(**values)
            with patch('local_files_mcp.os.fstat', side_effect=changed):
                with self.subTest(field=field), self.assertRaises(ValueError):
                    self.workspace.read_image(path.name)

    def test_root_replacement_missing_file_unknown_root_and_text_compatibility(self):
        self.fixture()
        with self.assertRaises(ValueError):
            self.workspace.read_image('image.png', root_id='unknown')
        with self.assertRaises(ValueError):
            self.workspace.read_image('missing.png')
        with self.assertRaises(ValueError):
            self.workspace.read_file('image.png')
        self.assertEqual(self.workspace.list_files()['files'], ['image.png'])
        self.reader._root_identity = (0, 0)
        with self.assertRaises(ValueError):
            self.workspace.read_image('image.png')

    def test_cancelled_operation_does_not_return_image(self):
        self.fixture()
        budget = Budget()
        with operation(budget):
            budget.cancel.set()
            with self.assertRaises(OperationError):
                self.workspace.read_image('image.png')

    def test_strict_arguments(self):
        server = create_server(self.workspace)
        for args in [{'path': 12}, {'path': ''}, {'path': 'image.png', 'write': True}, {'path': 'image.png', 'root_id': 1}]:
            with self.subTest(args=args), self.assertRaises(Exception):
                asyncio.run(server.call_tool('read_image', args))

    def test_real_stdio_image_content_contract_and_large_payload(self):
        self.fixture(size=(900, 900), noise=True)
        async def run():
            args = ['-B', str(Path('local_files_mcp.py').resolve()), '--root', str(self.root),
                    '--reader-settings', encode_reader_settings({'max_file_bytes': 16*1024*1024})]
            async with stdio_client(StdioServerParameters(command=sys.executable, args=args)) as (read, write):
                async with ClientSession(read, write) as session:
                    initialized = await session.initialize()
                    self.assertEqual(initialized.serverInfo.version, SERVICE_VERSION)
                    tools = (await session.list_tools()).tools
                    verify_contract(tools)
                    self.assertEqual(len(tools), 21)
                    result = await session.call_tool('read_image', {'path': 'image.png'})
                    self.assertFalse(result.isError)
                    self.assertGreater(len(result.content[1].data), 2*1024*1024)
                    self.assertEqual(self.decoded(result).size, (900, 900))
                    denied = await session.call_tool('read_image', {'path': '../image.png'})
                    self.assertTrue(denied.isError)
                    self.assertFalse(any(isinstance(c, ImageContent) for c in denied.content))
        asyncio.run(run())
