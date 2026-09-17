"""固定區塊驗證全文，只保留要求的有界行視窗。"""
import codecs
from stream_search import BREAKS
from operation_budget import checkpoint


def read_window(handle, maximum: int, start_line: int, line_count: int) -> dict:
    decoder = codecs.getincrementaldecoder('utf-8-sig')()
    used = number = length = chars = 0
    after_cr = False
    prefix = ''
    rendered = []
    end = 0
    full = False

    def append(part):
        nonlocal prefix, length
        length += len(part)
        if start_line <= number + 1 < start_line + line_count and not full:
            prefix = (prefix + part[:24001])[:24001]

    def finish():
        nonlocal number, prefix, length, chars, end, full
        number += 1
        if start_line <= number < start_line + line_count and not full:
            row = f'{number}: {prefix}'
            if length + len(str(number)) + 2 > 24000:
                raise ValueError('單行內容過長。請先將此檔案格式化或分割。')
            cost = len(row) + bool(rendered)
            if chars + cost > 24000:
                full = True
            else:
                rendered.append(row)
                chars += cost
                end = number
        prefix = ''
        length = 0

    while True:
        checkpoint()
        chunk = handle.read(min(65536, maximum + 1 - used))
        if not chunk:
            break
        used += len(chunk)
        checkpoint(bytes_read=len(chunk))
        if used > maximum or b'\x00' in chunk:
            raise ValueError('檔案過大或不是支援的文字格式。')
        try:
            text = decoder.decode(chunk)
        except UnicodeError:
            raise ValueError('僅支援 UTF-8 文字。') from None
        start = 0
        for separator in BREAKS.finditer(text):
            part = text[start:separator.start()]
            if part:
                after_cr = False
                append(part)
            char = separator.group()
            if not (after_cr and char == '\n'):
                finish()
            after_cr = char == '\r'
            start = separator.end()
        if start < len(text):
            after_cr = False
            append(text[start:])
    try:
        decoder.decode(b'', final=True)
    except UnicodeError:
        raise ValueError('僅支援 UTF-8 文字。') from None
    if length:
        finish()
    return {'total_lines': number, 'start_line': start_line, 'content': '\n'.join(rendered),
            'next_start_line': end + 1 if end and end < number else None}
