"""固定區塊 UTF-8 搜尋，超長行僅保留片段與比對尾端。"""
from operation_budget import checkpoint
import codecs
import os
import re
from collections import deque

BREAKS = re.compile(r'[\n\r\v\f\x1c-\x1e\x85\u2028\u2029]')


def scan_file(handle, maximum: int, budget: int, needle: str,
              limit: int, context: int) -> tuple:
    used, valid, groups = scan_file_many(handle, maximum, budget, [needle], [limit], context)
    return used, valid, groups[0]


def scan_file_many(handle, maximum: int, budget: int, needles: list[str],
                   limits: list[int], context: int) -> tuple:
    decoder = codecs.getincrementaldecoder('utf-8-sig')()
    used = 0
    valid = True
    snippets = [''] * len(needles)
    prefix = ''
    tails = [''] * len(needles)
    length = 0
    found = [False] * len(needles)
    after_cr = False
    number = 0
    previous = deque(maxlen=context)
    matches = [[] for _ in needles]
    pending = []

    def append(part):
        nonlocal prefix, length
        length += len(part)
        prefix = (prefix + part[:600])[:600]
        folded_part = part.casefold()
        for i, needle in enumerate(needles):
            folded = tails[i] + folded_part
            if not found[i] and needle in folded:
                position = folded.index(needle)
                begin = max(0, position - min(120, max(0, (600 - len(needle)) // 2)))
                snippets[i] = folded[begin:begin + 600]
                found[i] = True
            tails[i] = folded[-max(1, len(needle) - 1):]

    def finish():
        nonlocal prefix, length, number
        number += 1
        row = {'line': number, 'text': prefix, 'line_truncated': length > 600}
        for match in pending[:]:
            match['context'].append(dict(row))
            if number >= match['line'] + context:
                pending.remove(match)
        for i in range(len(needles)):
            if found[i] and len(matches[i]) < limits[i] + 1:
                match = dict(row)
                if length > 600:
                    match['match_snippet'] = snippets[i]
                    match['snippet_format'] = 'casefolded_text'
                if context:
                    match['context'] = [*previous, dict(row)]
                    pending.append(match)
                matches[i].append(match)
        previous.append(dict(row))
        prefix = ''
        length = 0
        found[:] = [False] * len(needles)
        tails[:] = [''] * len(needles)
        snippets[:] = [''] * len(needles)

    while used < min(maximum + 1, budget):
        checkpoint()
        chunk = handle.read(min(65536, maximum + 1 - used, budget - used))
        if not chunk:
            break
        used += len(chunk)
        checkpoint(bytes_read=len(chunk))
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
    return used, valid, matches if valid else [[] for _ in needles]
