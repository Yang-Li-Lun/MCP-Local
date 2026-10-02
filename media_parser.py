"""Bounded container metadata only; no decoders, URLs, or external commands."""
import io
import math
import struct
import json

from format_reader import sniff


def number(value: object) -> int | float | None:
    return value if isinstance(value, (int, float)) and math.isfinite(value) else None


def audio_metadata(data: bytes, fmt: str) -> dict:
    from mutagen.mp3 import MP3
    from mutagen.flac import FLAC
    from mutagen.wave import WAVE
    from mutagen.oggvorbis import OggVorbis
    from mutagen.oggopus import OggOpus
    from mutagen.mp4 import MP4
    from mutagen.aiff import AIFF
    parser = {'MP3': MP3, 'MPEG_AUDIO': MP3, 'FLAC': FLAC, 'WAV': WAVE, 'MP4': MP4, 'AIFF': AIFF}.get(fmt)
    if fmt == 'OGG':
        parser = OggOpus if b'OpusHead' in data[:512] else OggVorbis
    if parser is None:
        raise ValueError('MEDIA_FORMAT：不支援此音訊格式。')
    audio = parser(io.BytesIO(data))
    info = audio.info
    fields = {name: number(getattr(info, name, None)) for name in ('bitrate', 'sample_rate', 'channels', 'bits_per_sample')}
    fields.update(duration=number(getattr(info, 'length', None)), codec=getattr(info, 'codec', None) or parser.__name__)
    if parser is OggOpus:
        fields.update(sample_rate=48000, codec='Opus')
    tags = {}
    if audio.tags:
        for key, value in audio.tags.items():
            if len(tags) >= 32:
                break
            # Never serialize artwork/attachments or arbitrary object representations.
            if hasattr(value, 'text'):
                value = value.text
            values = value if isinstance(value, list) else [value]
            rendered = [str(v)[:1024] for v in values[:4] if isinstance(v, (str, int, float))]
            if rendered:
                tags[str(key)[:128]] = rendered
    return dict(format=fmt, tracks=[dict(type='audio', **fields)], tags=tags)


def mp4_metadata(data: bytes) -> dict:
    count = 0
    def boxes(start, end):
        nonlocal count
        result = []
        while start < end:
            count += 1
            if count > 10000 or end - start < 8:
                raise ValueError('MEDIA_CONTAINER_LIMIT：MP4 box 數量或界線無效。')
            size, kind = struct.unpack_from('>I4s', data, start)
            header = 8
            if size == 1:
                if end - start < 16:
                    raise ValueError('MEDIA_CONTAINER：box header 損毀。')
                size = struct.unpack_from('>Q', data, start+8)[0]
                header = 16
            elif size == 0:
                size = end - start
            if size < header or size > end - start:
                raise ValueError('MEDIA_CONTAINER：box size 損毀。')
            result.append((kind, start+header, start+size))
            start += size
        return result

    def children(box):
        return boxes(box[1], box[2])

    def required(items, name):
        found = [b for b in items if b[0] == name]
        if len(found) != 1:
            raise ValueError('MEDIA_CONTAINER：必要 box 遺失或重複。')
        return found[0]

    def payload(box, minimum):
        if box[2] - box[1] < minimum:
            raise ValueError('MEDIA_CONTAINER：box 內容截斷。')
        return data[box[1]:box[2]]

    top = boxes(0, len(data))
    payload(required(top, b'ftyp'), 8)
    moov = children(required(top, b'moov'))
    tracks = []
    for trak in [b for b in moov if b[0] == b'trak']:
        if len(tracks) >= 32:
            raise ValueError('MEDIA_TRACK_LIMIT：軌道超限。')
        track = children(trak)
        mdia = children(required(track, b'mdia'))
        mdhd = payload(required(mdia, b'mdhd'), 24)
        if mdhd[0] == 1:
            if len(mdhd) < 36:
                raise ValueError('MEDIA_CONTAINER：mdhd 截斷。')
            scale = struct.unpack_from('>I', mdhd, 20)[0]
            duration = struct.unpack_from('>Q', mdhd, 24)[0]
        elif mdhd[0] == 0:
            scale, duration = struct.unpack_from('>II', mdhd, 12)
        else:
            raise ValueError('MEDIA_CONTAINER：mdhd 版本不支援。')
        handler = payload(required(mdia, b'hdlr'), 12)[8:12]
        row = dict(type={b'vide': 'video', b'soun': 'audio'}.get(handler, 'other'),
                   duration=duration/scale if scale else None)
        minf = children(required(mdia, b'minf'))
        stbl = children(required(minf, b'stbl'))
        stsd = required(stbl, b'stsd')
        description = payload(stsd, 8)
        entries = boxes(stsd[1]+8, stsd[2])
        if len(entries) != struct.unpack_from('>I', description, 4)[0] or not 1 <= len(entries) <= 32:
            raise ValueError('MEDIA_CONTAINER：sample descriptions 無效。')
        row['codecs'] = [b[0].decode('ascii', 'replace') for b in entries]
        if handler == b'vide':
            entry = payload(entries[0], 28)
            row['width'], row['height'] = struct.unpack_from('>HH', entry, 24)
        elif handler == b'soun':
            entry = payload(entries[0], 28)
            row['channels'] = struct.unpack_from('>H', entry, 16)[0]
            row['sample_rate'] = struct.unpack_from('>I', entry, 24)[0] / 65536
        timing = next((b for b in stbl if b[0] == b'stts'), None)
        if timing:
            timing_data = payload(timing, 8)
            n = struct.unpack_from('>I', timing_data, 4)[0]
            if n > 100000 or len(timing_data) != 8 + n*8:
                raise ValueError('MEDIA_CONTAINER_LIMIT：時間表超限或截斷。')
            samples = ticks = 0
            for offset in range(8, len(timing_data), 8):
                amount, delta = struct.unpack_from('>II', timing_data, offset)
                samples += amount
                ticks += amount * delta
            if handler == b'vide':
                row['fps'] = samples * scale / ticks if ticks else None
        tracks.append(row)
    if not tracks:
        raise ValueError('MEDIA_CONTAINER：沒有可辨識軌道。')
    duration = max((t['duration'] or 0 for t in tracks), default=0)
    tags = {}
    if any(t['type'] == 'audio' for t in tracks):
        try:
            extra = audio_metadata(data, 'MP4')
            tags = extra['tags']
            audio = next(t for t in tracks if t['type'] == 'audio')
            for key in ('bitrate', 'sample_rate', 'channels'):
                if extra['tracks'][0].get(key) is not None:
                    audio[key] = extra['tracks'][0][key]
        except (ValueError, OSError, KeyError):
            pass
    return dict(format='MP4', tracks=tracks, duration=duration, tags=tags,
                bitrate=len(data)*8/duration if duration else None, bitrate_kind='whole-file average',
                fragmented='moof' in [b[0].decode('ascii', 'replace') for b in top])


def avi_metadata(data: bytes) -> dict:
    if len(data) < 12 or data[:4] != b'RIFF' or data[8:12] != b'AVI ' or struct.unpack_from('<I', data, 4)[0]+8 != len(data):
        raise ValueError('MEDIA_CONTAINER：AVI header 無效。')
    count = 0
    def chunks(start, end, depth=0):
        nonlocal count
        while start < end:
            count += 1
            if count > 10000 or depth > 8 or end-start < 8:
                raise ValueError('MEDIA_CONTAINER_LIMIT：AVI chunk 超限。')
            kind, size = struct.unpack_from('<4sI', data, start)
            stop = start+8+size
            if stop > end:
                raise ValueError('MEDIA_CONTAINER：AVI chunk 截斷。')
            if kind == b'LIST' and size >= 4 and data[start+8:start+12] != b'movi':
                yield from chunks(start+12, stop, depth+1)
            else:
                yield kind, data[start+8:stop]
            start = stop + (size & 1)
    tracks = []
    header = None
    for kind, content in chunks(12, len(data)):
        if kind == b'avih' and len(content) >= 56:
            header = struct.unpack_from('<14I', content)
        elif kind == b'strh' and len(content) >= 56:
            if len(tracks) >= 32:
                raise ValueError('MEDIA_TRACK_LIMIT：AVI 軌道超限。')
            scale, rate, _, length = struct.unpack_from('<4I', content, 20)
            tracks.append(dict(type={b'vids': 'video', b'auds': 'audio'}.get(content[:4], 'other'),
                               codec=content[4:8].decode('ascii', 'replace'),
                               duration=length*scale/rate if rate else None,
                               fps=rate/scale if scale and content[:4] == b'vids' else None))
        elif kind == b'strf' and tracks and tracks[-1]['type'] == 'audio' and len(content) >= 16:
            codec, channels, rate, avg, _, bits = struct.unpack_from('<HHIIHH', content)
            tracks[-1].update(codec=f'WAVE:{codec}', channels=channels, sample_rate=rate, bitrate=avg*8, bits_per_sample=bits)
    if header is None:
        raise ValueError('MEDIA_CONTAINER：AVI 缺少主 header。')
    for row in tracks:
        if row['type'] == 'video':
            row.update(width=header[8], height=header[9])
    return dict(format='AVI', tracks=tracks, duration=header[4]*header[0]/1000000,
                width=header[8], height=header[9], fps=1000000/header[0] if header[0] else None,
                tags={}, bitrate=header[1]*8, bitrate_kind='declared maximum')


def matroska_metadata(data: bytes) -> dict:
    """Read fixed EBML metadata fields, skipping media payload and attachments."""
    count = 0

    def vint(pos, end, identifier=False):
        if pos >= end or data[pos] == 0:
            raise ValueError('MEDIA_EBML：EBML 整數截斷或無效。')
        width = 9 - data[pos].bit_length()
        if width > (4 if identifier else 8) or pos + width > end:
            raise ValueError('MEDIA_EBML：EBML 整數長度無效。')
        raw = int.from_bytes(data[pos:pos+width], 'big')
        value = raw if identifier else raw & ((1 << (7*width)) - 1)
        return value, width, not identifier and value == (1 << (7*width)) - 1

    def children(start, end):
        nonlocal count
        result = []
        while start < end:
            count += 1
            if count > 10000:
                raise ValueError('MEDIA_CONTAINER_LIMIT：EBML 元素數超限。')
            tag, width, _ = vint(start, end, True)
            size, size_width, unknown = vint(start + width, end)
            body = start + width + size_width
            if unknown:
                if tag != 0x18538067:
                    raise ValueError('MEDIA_EBML：僅允許最外層 Segment 的未知長度。')
                size = end - body
            if size > end - body:
                raise ValueError('MEDIA_EBML：元素內容截斷。')
            result.append((tag, body, body + size))
            start = body + size
        return result

    def fields(items):
        result = {}
        for tag, begin, end in items:
            if tag in (0xec, 0xbf):  # Void / optional CRC; metadata-only, no CRC claim.
                continue
            if tag in result:
                raise ValueError('MEDIA_EBML：metadata 欄位重複。')
            result[tag] = (begin, end)
        return result

    def payload(items, tag, maximum=256):
        if tag not in items:
            return None
        begin, end = items[tag]
        if end - begin > maximum:
            raise ValueError('MEDIA_EBML：metadata 欄位超限。')
        return data[begin:end]

    def uint(items, tag, default=None):
        raw = payload(items, tag, 8)
        if raw == b'':
            return 0
        return int.from_bytes(raw, 'big') if raw is not None else default

    def floating(items, tag, default=None):
        raw = payload(items, tag, 8)
        if raw is None:
            return default
        if len(raw) not in (4, 8):
            raise ValueError('MEDIA_EBML：浮點欄位無效。')
        value = struct.unpack('>f' if len(raw) == 4 else '>d', raw)[0]
        if not math.isfinite(value) or value < 0:
            raise ValueError('MEDIA_EBML：浮點欄位不是有限正值。')
        return value

    top = fields(children(0, len(data)))
    if 0x1a45dfa3 not in top or 0x18538067 not in top:
        raise ValueError('MEDIA_EBML：缺少 EBML header 或 Segment。')
    header = fields(children(*top[0x1a45dfa3]))
    doctype = payload(header, 0x4282)
    if doctype not in (b'matroska', b'webm'):
        raise ValueError('MEDIA_EBML：不支援的 DocType。')
    segment = children(*top[0x18538067])
    selected = fields([entry for entry in segment if entry[0] in (0x1549a966, 0x1654ae6b)])
    if 0x1549a966 not in selected or 0x1654ae6b not in selected:
        raise ValueError('MEDIA_EBML：缺少 Info 或 Tracks。')
    info = fields(children(*selected[0x1549a966]))
    scale = uint(info, 0x2ad7b1, 1000000)
    duration = floating(info, 0x4489)
    if not scale:
        raise ValueError('MEDIA_EBML：時間尺度無效。')
    tracks = []
    for tag, begin, end in children(*selected[0x1654ae6b]):
        if tag != 0xae:
            continue
        if len(tracks) >= 32:
            raise ValueError('MEDIA_TRACK_LIMIT：軌道數超限。')
        track = fields(children(begin, end))
        codec = payload(track, 0x86)
        if codec is None:
            raise ValueError('MEDIA_EBML：缺少 CodecID。')
        row = dict(number=uint(track, 0xd7), type={1: 'video', 2: 'audio', 17: 'subtitle'}.get(uint(track, 0x83), 'other'),
                   codec=codec.decode('ascii'), language=(payload(track, 0x22b59c) or b'eng').decode('ascii'))
        if 0xe0 in track:
            video = fields(children(*track[0xe0]))
            row.update(width=uint(video, 0xb0), height=uint(video, 0xba))
        if 0xe1 in track:
            audio = fields(children(*track[0xe1]))
            row.update(sample_rate=floating(audio, 0xb5, 8000), channels=uint(audio, 0x9f, 1), bits_per_sample=uint(audio, 0x6264))
        tracks.append(row)
    if not tracks:
        raise ValueError('MEDIA_EBML：沒有軌道。')
    return dict(format='WEBM' if doctype == b'webm' else 'MATROSKA', tracks=tracks,
                duration=duration * scale / 1000000000 if duration is not None else None,
                validation='bounded metadata only; frames and EBML CRC not validated', tags={})


def media(data: bytes) -> dict:
    fmt, _ = sniff(data)
    if fmt in ('PNG', 'JPEG', 'WEBP', 'GIF', 'BMP', 'TIFF', 'ICO', 'PPM', 'AVIF'):
        from image_reader import decode_image, MAX_SOURCE_BYTES
        if len(data) > MAX_SOURCE_BYTES:
            raise ValueError('MEDIA_IMAGE_LIMIT：圖片來源超過 16 MiB。')
        decoded = decode_image(data, fmt)
        return dict(format=fmt, kind='static image', **json.loads(decoded.content[0].text))
    if fmt == 'MATROSKA':
        return matroska_metadata(data)
    if fmt == 'AU':
        if len(data) < 24:
            raise ValueError('MEDIA_CONTAINER：AU header 截斷。')
        offset, size, encoding, rate, channels = struct.unpack_from('>5I', data, 4)
        bits = {1: 8, 2: 8, 3: 16, 4: 24, 5: 32, 6: 32, 7: 64, 27: 8}.get(encoding)
        if offset < 24 or offset > len(data) or not rate or not 1 <= channels <= 32 or bits is None:
            raise ValueError('MEDIA_CONTAINER：AU header 無效或不支援此編碼。')
        actual = len(data) - offset
        if size not in (actual, 0xffffffff) or actual % (channels * (bits // 8)):
            raise ValueError('MEDIA_CONTAINER：AU 長度無效。')
        return dict(format=fmt, tracks=[dict(type='audio', codec=f'AU:{encoding}', bits_per_sample=bits,
                    sample_rate=rate, channels=channels, duration=actual / (rate * channels * (bits // 8)))], tags={})
    if fmt == 'MP4':
        return mp4_metadata(data)
    if fmt == 'AVI':
        return avi_metadata(data)
    return audio_metadata(data, fmt)
