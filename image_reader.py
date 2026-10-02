"""Bounded, memory-only image decoding through the shared checked-open guard."""
import base64
import io
import json

from PIL import Image, ImageOps, UnidentifiedImageError, features
from mcp.types import CallToolResult, ImageContent, TextContent
from operation_budget import checkpoint

IMAGE_EXTENSIONS = {'.png': 'PNG', '.jpg': 'JPEG', '.jpeg': 'JPEG', '.webp': 'WEBP',
                    '.gif': 'GIF', '.bmp': 'BMP', '.tif': 'TIFF', '.tiff': 'TIFF',
                    '.ico': 'ICO', '.pbm': 'PPM', '.pgm': 'PPM', '.ppm': 'PPM', '.pnm': 'PPM'}
if features.check('avif'):
    IMAGE_EXTENSIONS['.avif'] = 'AVIF'
MAX_SOURCE_BYTES = 16 * 1024 * 1024
MAX_SOURCE_SIDE = 16384
MAX_SOURCE_PIXELS = 25_000_000
MAX_OUTPUT_SIDE = 2048
MAX_IMAGE_BYTES = 3 * 1024 * 1024
MAX_BASE64_BYTES = 4 * ((MAX_IMAGE_BYTES + 2) // 3)
MAX_IMAGE_RESPONSE_BYTES = MAX_BASE64_BYTES + 64 * 1024


def image_limits() -> dict:
    return dict(formats=['PNG', 'JPEG', 'WebP', 'GIF', 'BMP', 'TIFF', 'ICO', 'PNM (P1-P6)'] +
                       (['AVIF'] if 'AVIF' in IMAGE_EXTENSIONS.values() else []), output_mime_type='image/png',
                avif_available='AVIF' in IMAGE_EXTENSIONS.values(),
                max_source_bytes=MAX_SOURCE_BYTES, source_bytes_policy='min(max_source_bytes, root.max_file_bytes, root.max_scan_bytes)',
                max_source_side=MAX_SOURCE_SIDE, max_source_pixels=MAX_SOURCE_PIXELS,
                max_output_side=MAX_OUTPUT_SIDE, max_image_bytes=MAX_IMAGE_BYTES,
                max_base64_bytes=MAX_BASE64_BYTES, max_response_bytes=MAX_IMAGE_RESPONSE_BYTES,
                animation='rejected', metadata='stripped', resize='aspect ratio preserved; never upscale',
                parser_timeout_seconds=10, parser_memory_bytes=512*1024*1024)


class _EncodedLimit(ValueError):
    pass


class _Output(io.BytesIO):
    def write(self, data: bytes) -> int:
        if self.tell() + len(data) > MAX_IMAGE_BYTES:
            raise _EncodedLimit()
        return super().write(data)


def read_image(reader, relative: str) -> CallToolResult:
    path = reader.checked(relative)
    expected = IMAGE_EXTENSIONS.get(path.suffix.casefold())
    if expected is None:
        raise ValueError('圖片僅支援 image_limits 公開且符合實際內容的副檔名。')
    maximum = min(MAX_SOURCE_BYTES, reader.settings['max_file_bytes'], reader.settings['max_scan_bytes'])
    with reader.open_checked(path, image=True) as handle:
        source = handle.read(maximum + 1)
        checkpoint(bytes_read=len(source))
        if len(source) > maximum:
            raise ValueError('圖片超過來源容量上限。')
    from format_reader import parse_snapshot
    return CallToolResult.model_validate(parse_snapshot('image', source, expected=expected))


def decode_image(source: bytes, expected: str) -> CallToolResult:
    """Only called inside the resource-limited parser worker."""
    try:
        # Limit plugin dispatch as well as validating the decoded format.
        with Image.open(io.BytesIO(source), formats=[expected]) as probe:
            width, height = probe.size
            if (probe.format != expected or width < 1 or height < 1 or
                    max(width, height) > MAX_SOURCE_SIDE or width * height > MAX_SOURCE_PIXELS):
                raise ValueError('圖片格式、解析度或像素數超出限制。')
            if getattr(probe, 'n_frames', 1) != 1:
                raise ValueError('第一版不支援動畫或多幀圖片。')
            probe.verify()
        checkpoint()
        with Image.open(io.BytesIO(source), formats=[expected]) as decoded:
            decoded.load()  # Never accept header-only or truncated image data.
            with ImageOps.exif_transpose(decoded) as oriented:
                oriented.thumbnail((MAX_OUTPUT_SIDE, MAX_OUTPUT_SIDE), Image.Resampling.LANCZOS)
                mode = 'RGBA' if 'A' in oriented.getbands() or 'transparency' in oriented.info else 'RGB'
                with oriented.convert(mode) as pixels:
                    # Copy pixels only, so ICC/EXIF/comments/text cannot enter the response.
                    output = Image.frombytes(mode, pixels.size, pixels.tobytes())
        try:
            while True:
                checkpoint()
                try:
                    with _Output() as buffer:
                        output.save(buffer, format='PNG')
                        encoded = buffer.getvalue()
                    break
                except _EncodedLimit:
                    if max(output.size) <= 1:
                        raise ValueError('圖片無法縮小至傳輸限制內。') from None
                    size = tuple(max(1, int(side * .75)) for side in output.size)
                    smaller = output.resize(size, Image.Resampling.LANCZOS)
                    output.close()
                    output = smaller
            metadata = dict(source_format=expected, source_width=width, source_height=height,
                            width=output.width, height=output.height,
                            source_bytes=len(source), image_bytes=len(encoded),
                            resized=(width, height) != output.size, metadata_stripped=True)
        finally:
            output.close()
    except (UnidentifiedImageError, OSError, SyntaxError, Image.DecompressionBombError):
        raise ValueError('圖片損毀、格式偽裝或超出安全解碼限制。') from None
    checkpoint()
    return CallToolResult(content=[
        TextContent(type='text', text=json.dumps(metadata, ensure_ascii=False)),
        ImageContent(type='image', mimeType='image/png', data=base64.b64encode(encoded).decode('ascii'))])
