"""Fixed parser worker. No source paths, persistence, network or child processes."""
import base64
import json
import os
from pathlib import Path
import sys

# -I deliberately omits script directories; add only the installed source directory.
sys.path.insert(0, str(Path(__file__).resolve().parent))


def memory_limit() -> None:
    maximum = 512 * 1024 * 1024
    if os.name != 'nt':
        import resource
        resource.setrlimit(resource.RLIMIT_AS, (maximum, maximum))
        resource.setrlimit(resource.RLIMIT_CPU, (10, 10))
        return
    import ctypes
    from ctypes import wintypes

    class Basic(ctypes.Structure):
        _fields_ = [('process_time', ctypes.c_longlong), ('job_time', ctypes.c_longlong),
                    ('flags', wintypes.DWORD), ('min_ws', ctypes.c_size_t), ('max_ws', ctypes.c_size_t),
                    ('active', wintypes.DWORD), ('affinity', ctypes.c_size_t),
                    ('priority', wintypes.DWORD), ('scheduling', wintypes.DWORD)]

    class Io(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in ('r', 'w', 'o', 'rb', 'wb', 'ob')]

    class Extended(ctypes.Structure):
        _fields_ = [('basic', Basic), ('io', Io), ('process_memory', ctypes.c_size_t),
                    ('job_memory', ctypes.c_size_t), ('peak_process', ctypes.c_size_t), ('peak_job', ctypes.c_size_t)]

    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    kernel.CreateJobObjectW.argtypes = [ctypes.c_void_p, wintypes.LPCWSTR]
    kernel.CreateJobObjectW.restype = wintypes.HANDLE
    kernel.SetInformationJobObject.argtypes = [wintypes.HANDLE, ctypes.c_int, ctypes.c_void_p, wintypes.DWORD]
    kernel.SetInformationJobObject.restype = wintypes.BOOL
    kernel.GetCurrentProcess.restype = wintypes.HANDLE
    kernel.AssignProcessToJobObject.argtypes = [wintypes.HANDLE, wintypes.HANDLE]
    kernel.AssignProcessToJobObject.restype = wintypes.BOOL
    job = kernel.CreateJobObjectW(None, None)
    info = Extended()
    info.basic.flags = 0x100 | 0x8 | 0x2  # memory, process count, CPU time
    info.basic.active = 1
    info.basic.process_time = 10 * 10000000  # 100-nanosecond units
    info.process_memory = maximum
    if (not job or not kernel.SetInformationJobObject(job, 9, ctypes.byref(info), ctypes.sizeof(info))
            or not kernel.AssignProcessToJobObject(job, kernel.GetCurrentProcess())):
        raise ValueError('FORMAT_ISOLATION：無法建立解析資源限制。')
    # Hold the dedicated parser Job handle for this process's lifetime.


def deny_external_access(event: str, args: tuple) -> None:
    if event == 'sqlite3.connect' and args[0] != ':memory:':
        raise ValueError('FORMAT_EXTERNAL_ACCESS：SQLite 僅限記憶體。')
    if event in ('sqlite3.load_extension', 'sqlite3.enable_load_extension'):
        raise ValueError('FORMAT_EXTERNAL_ACCESS：禁止 SQLite extension。')
    if (event == 'open' or event.startswith(('socket.', 'subprocess.', 'os.exec', 'os.spawn'))
            or event in ('os.system', 'os.remove', 'os.rename', 'os.mkdir', 'os.rmdir', 'ctypes.dlopen')):
        raise ValueError('FORMAT_EXTERNAL_ACCESS：解析不得存取檔案、網路或執行程式。')


def main() -> None:
    try:
        memory_limit()
        from format_reader import MAX_SOURCE, MAX_RESPONSE
        from format_parsers import document, archive_info, checked_zip, office_kind
        from media_parser import media
        from document_extras import zip_kind, email_document
        from structured_parser import detect_text, structured, sqlite_supported
        # Load parser modules/plugins before denying file/network activity.
        from pypdf import PdfReader, overwrite_configuration
        overwrite_configuration(maximum_declared_stream_length=8*1024*1024,
                                array_based_stream_maximum_output_length=8*1024*1024,
                                jbig2_maximum_output_length=8*1024*1024,
                                lzw_maximum_output_length=8*1024*1024,
                                run_length_maximum_output_length=8*1024*1024,
                                zlib_maximum_output_length=8*1024*1024,
                                image_maximum_buffer_size=8*1024*1024,
                                page_tree_maximum_entries=4000, page_tree_maximum_depth=64,
                                xform_maximum_invocations_per_extraction=1000, jbig2dec_binary=None)
        from mutagen.mp3 import MP3
        from mutagen.flac import FLAC
        from mutagen.wave import WAVE
        from mutagen.mp4 import MP4
        from mutagen.oggvorbis import OggVorbis
        from mutagen.oggopus import OggOpus
        from mutagen.aiff import AIFF
        from image_reader import decode_image
        from PIL import Image
        Image.init()
        # ZIP filenames without the UTF-8 flag use a lazily imported codec.
        import encodings.cp437
        import encodings.utf_8_sig
        # Fixed charset allowlist; no input-selected module import after isolation.
        import codecs
        for charset in ('utf-8', 'ascii', 'iso-8859-1', 'windows-1252', 'big5', 'cp950', 'gb18030', 'shift_jis', 'iso-2022-jp'):
            codecs.lookup(charset)

        sys.addaudithook(deny_external_access)
        raw = sys.stdin.buffer.read(4*((MAX_SOURCE+2)//3) + 65536)
        request = json.loads(raw)
        data = base64.b64decode(request['data'], validate=True)
        if len(data) > MAX_SOURCE:
            raise ValueError('FORMAT_SOURCE_LIMIT：來源超限。')
        options = request['options']
        kind = request['kind']
        maximum = MAX_RESPONSE
        if kind == 'document':
            result = document(data, options)
        elif kind == 'archive':
            result = archive_info(data, options)
        elif kind == 'media':
            result = media(data)
        elif kind == 'identify':
            from format_reader import sniff
            if sniff(data)[0] == 'ZIP':
                with checked_zip(data) as archive:
                    archive._reader_exclusions = options.get('excluded_names', [])
                    result = dict(format=zip_kind(archive))
            else:
                fmt = detect_text(data)
                validation = dict(options, start=0, limit=1)
                if fmt in ('EML', 'MBOX'):
                    email_document(data, fmt, validation)
                elif fmt != 'UTF-8':
                    structured(data, fmt, validation)
                result = dict(format=fmt, detection='complete bounded UTF-8/structure validation; ambiguous formats require format_hint')
        elif kind == 'image':
            result = decode_image(data, options['expected']).model_dump(by_alias=True, exclude_none=True)
            maximum = 5 * 1024 * 1024
        else:
            raise ValueError('FORMAT_OPERATION：不支援的解析操作。')
        output = json.dumps(result, ensure_ascii=False, allow_nan=False).encode('utf-8')
        if len(output) > maximum:
            raise ValueError('FORMAT_OUTPUT_LIMIT：解析輸出超限。')
    except Exception as exc:
        # Parser exceptions may contain document content; return only our fixed codes.
        message = str(exc)
        if not isinstance(exc, ValueError) or not message.startswith(('FORMAT_', 'ARCHIVE_', 'DOCUMENT_', 'MEDIA_')):
            message = 'FORMAT_INVALID：檔案損毀、格式不符、不支援或超出解析限制。'
        output = json.dumps({'error': message}, ensure_ascii=False).encode('utf-8')
    sys.stdout.buffer.write(output)


if __name__ == '__main__':
    main()
