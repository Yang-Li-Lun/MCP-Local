"""Desktop Commander 精確替換與換行演算法的 Python 移植。

MIT: Copyright (c) 2024-2025 Eduard Ruzga and Desktop Commander Contributors.
來源與修改說明見 third_party/DesktopCommanderMCP/NOTICE；保留完整 MIT 授權。
"""
import os


def detect_line_ending(content: str) -> str:
    for index, char in enumerate(content):
        if char == '\r':
            return '\r\n' if content[index:index + 2] == '\r\n' else '\r'
        if char == '\n':
            return '\n'
    return '\r\n' if os.name == 'nt' else '\n'


def normalize_line_endings(content: str, ending: str) -> str:
    return content.replace('\r\n', '\n').replace('\r', '\n').replace('\n', ending)


def replace_block(content: str, old: str, new: str, expected: int) -> str:
    if not old or type(expected) is not int or not 1 <= expected <= 1000:
        raise ValueError('EDIT_INVALID：搜尋文字不可為空，替換次數須為 1 至 1000。')
    ending = detect_line_ending(content)
    search = normalize_line_endings(old, ending)
    # As in upstream, count overlapping matches too; ambiguity must never write.
    count, position = 0, content.find(search)
    while position >= 0:
        count += 1
        position = content.find(search, position + 1)
    if count != expected or content.count(search) != expected:
        raise ValueError(f'EDIT_MATCH_COUNT：預期 {expected} 次，找到 {count} 次；檔案未變更。')
    return content.replace(search, normalize_line_endings(new, ending))
