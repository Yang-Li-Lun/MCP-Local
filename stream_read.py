"""固定區塊驗證全文，單次讀取保留多個有界行視窗。"""
import codecs
import json
from stream_search import BREAKS
from operation_budget import checkpoint

MAX_READ_RANGES = 16
RANGES_BYTES = 512 * 1024


def validate_ranges(ranges: list) -> None:
    if type(ranges) is not list or not 1 <= len(ranges) <= MAX_READ_RANGES:
        raise ValueError('ranges 必須包含 1 至 16 個行區段。')
    for item in ranges:
        if (type(item) is not dict or set(item) != {'start_line', 'line_count'}
                or type(item['start_line']) is not int or item['start_line'] < 1
                or type(item['line_count']) is not int or not 1 <= item['line_count'] <= 400):
            raise ValueError('每區段須指定 start_line >= 1 及 line_count 1 至 400。')


def read_ranges(handle, maximum: int, ranges: list,
                output_budget: int = RANGES_BYTES - 32768) -> dict:
    validate_ranges(ranges)
    decoder = codecs.getincrementaldecoder('utf-8-sig')()
    states = [dict(index=i, start=r['start_line'], stop=r['start_line'] + r['line_count'],
                   rendered=[], chars=0, end=0, full=False) for i, r in enumerate(ranges)]
    waiting = sorted(states, key=lambda r: r['start'], reverse=True)
    active = []
    used = number = length = 0
    after_cr = False
    prefix = ''

    def activate():
        while waiting and waiting[-1]['start'] <= number + 1:
            active.append(waiting.pop())

    activate()

    def append(part):
        nonlocal prefix, length
        length += len(part)
        if active:
            prefix = (prefix + part[:24001])[:24001]

    def finish():
        nonlocal number, prefix, length
        number += 1
        if active:
            row = f'{number}: {prefix}'
            if length + len(str(number)) + 2 > 24000:
                raise ValueError('單行內容過長。請先將此檔案格式化或分割。')
            for state in active:
                cost = len(row) + bool(state['rendered'])
                if state['chars'] + cost > 24000:
                    state['full'] = True
                else:
                    state['rendered'].append(row)
                    state['chars'] += cost
                    state['end'] = number
            active[:] = [s for s in active if not s['full'] and number + 1 < s['stop']]
        prefix = ''
        length = 0
        activate()

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
    results = []
    truncated = False
    # 預留封套和每個區段的 metadata；逐段只計算一次 pretty JSON 成本。
    output_used = 1024 + sum(512 + len(json.dumps(
        {'start_line': s['start'], 'next_start_line': s['start']}).encode('utf-8')) for s in states)
    for state in states:
        rows = state['rendered']
        content = '\n'.join(rows)
        cost = len(json.dumps(content, ensure_ascii=False).encode('utf-8'))
        end = state['end']
        next_line = end + 1 if end and end < number else None
        if output_used + cost > output_budget:
            content = ''
            rows = []
            next_line = state['start'] if state['start'] <= number else None
            truncated = True
        else:
            output_used += cost
        truncated |= state['full']
        results.append(dict(index=state['index'], start_line=state['start'],
                            returned_lines=len(rows), content=content, next_start_line=next_line))
    return dict(total_lines=number, ranges=results, truncated=truncated)


def read_window(handle, maximum: int, start_line: int, line_count: int) -> dict:
    result = read_ranges(handle, maximum, [dict(start_line=start_line, line_count=line_count)])
    row = result['ranges'][0]
    return {key: row[key] for key in ('start_line', 'content', 'next_start_line')} | {
        'total_lines': result['total_lines']}
