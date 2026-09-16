"""固定區塊 UTF-8 搜尋，超長行僅保留片段與比對尾端。"""
import codecs
import os
import re
from collections import deque

BREAKS = re.compile(r'[\n\r\v\f\x1c-\x1e\x85\u2028\u2029]')


def scan_file(handle, maximum: int, budget: int, needle: str,
              limit: int, context: int) -> tuple:
    decoder = codecs.getincrementaldecoder('utf-8-sig')()
    used = 0
    valid = True
    prefix = tail = ''
    length = 0
    found = False
    after_cr = False
    number = 0
    previous = deque(maxlen=context)
    matches = []
    pending = []

    def append(part):
        nonlocal prefix, tail, length, found
        length += len(part)
        prefix = (prefix + part[:600])[:600]
        folded = tail + part.casefold()
        found = found or needle in folded
        tail = folded[-max(1, len(needle) - 1):]

    def finish():
        nonlocal prefix, tail, length, found, number
        number += 1
        row = {'line': number, 'text': prefix, 'line_truncated': length > 600}
        for match in pending[:]:
            match['context'].append(dict(row))
            if number >= match['line'] + context:
                pending.remove(match)
        if found and len(matches) < limit + 1:
            match = dict(row)
            if context:
                match['context'] = [*previous, dict(row)]
                pending.append(match)
            matches.append(match)
        previous.append(dict(row))
        prefix = tail = ''
        length = 0
        found = False

    while used < min(maximum + 1, budget):
        chunk = handle.read(min(65536, maximum + 1 - used, budget - used))
        if not chunk:
            break
        used += len(chunk)
        if b'\x00' in chunk:
            valid = False
        try:
            text = decoder.decode(chunk)
        except UnicodeError:
            valid = False
            continue
        if not valid:
            continue
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
        valid = False
    complete = used >= os.fstat(handle.fileno()).st_size
    valid = valid and complete and used <= maximum
    if length and valid:
        finish()
    return used, valid, matches if valid else []
