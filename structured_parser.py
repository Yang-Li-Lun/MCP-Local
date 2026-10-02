"""Bounded structured data from memory, never user SQL, paths or loaders."""
import base64
import configparser
import csv
import io
import json
import math
import sqlite3
import yaml
import re
from itertools import islice

try:
    import tomllib
except ImportError:  # Python 3.10 retains the other readers.
    tomllib = None

from format_parsers import MAX_UNITS, paginate, xml

TEXT_FORMATS = ('JSON', 'JSONL', 'CSV', 'TSV', 'XML', 'YAML', 'TOML', 'INI', 'EML', 'MBOX', 'SRT', 'VTT')


class DataLoader(yaml.SafeLoader):
    """Pure Python SafeLoader plus bounded, string-only, duplicate-free mappings."""
    def construct_mapping(self, node: yaml.MappingNode, deep: bool = False) -> dict:
        mapping = {}
        for key_node, value_node in node.value:
            key = self.construct_object(key_node, deep=deep)
            if not isinstance(key, str) or key in mapping or key == '<<':
                raise ValueError('DOCUMENT_YAML：只支援不重複的字串 key；禁止 merge。')
            mapping[key] = self.construct_object(value_node, deep=deep)
        return mapping


def yaml_value(text: str) -> object:
    depth = nodes = documents = 0
    # Reject expansion and custom tags before constructing any Python values.
    for event in yaml.parse(text, Loader=yaml.SafeLoader):
        if isinstance(event, yaml.AliasEvent) or getattr(event, 'anchor', None) or getattr(event, 'tag', None):
            raise ValueError('DOCUMENT_YAML：禁止 anchor、alias 與明確 tag。')
        if isinstance(event, yaml.DocumentStartEvent):
            documents += 1
            if documents > 1:
                raise ValueError('DOCUMENT_YAML：一次只讀一份 YAML 文件。')
        if isinstance(event, (yaml.MappingStartEvent, yaml.SequenceStartEvent, yaml.ScalarEvent)):
            nodes += 1
            if nodes > MAX_UNITS or len(getattr(event, 'value', '')) > 128 * 1024:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：YAML 節點或 scalar 超限。')
        if isinstance(event, (yaml.MappingStartEvent, yaml.SequenceStartEvent)):
            depth += 1
            if depth > 64:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：YAML 深度超限。')
        elif isinstance(event, (yaml.MappingEndEvent, yaml.SequenceEndEvent)):
            depth -= 1
    return yaml.load(text, Loader=DataLoader)


def text_source(data: bytes) -> str:
    text = data.decode('utf-8-sig')
    if '\0' in text:
        raise ValueError('DOCUMENT_TEXT：文字含 NUL。')
    return text


def unique_object(pairs: list) -> dict:
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('DOCUMENT_JSON：重複 JSON key。')
        result[key] = value
    return result


def bad_constant(value: str) -> None:
    raise ValueError('DOCUMENT_JSON：非有限數字。')


def load_json(text: str) -> object:
    return json.loads(text, object_pairs_hook=unique_object, parse_constant=bad_constant)


def detect_text(data: bytes) -> str:
    """Conservative content candidates; callers still validate the entire input."""
    text = text_source(data)
    stripped = text.lstrip()
    if text.startswith('WEBVTT'):
        return 'VTT'
    if re.match(r'^\d+\r?\n\d{2,}:\d{2}:\d{2},\d{3} --> ', text):
        return 'SRT'
    if stripped.startswith('<'):
        return 'XML'
    if stripped.startswith(('---\n', '%YAML ')):
        return 'YAML'
    if text.startswith('From '):
        return 'MBOX'
    header = text.replace('\r\n', '\n').split('\n\n', 1)[0].lower()
    if any(header.startswith(key) or '\n' + key in header for key in ('from:', 'subject:', 'mime-version:')):
        return 'EML'
    if stripped.startswith(('{', '[')):
        try:
            load_json(text)
            return 'JSON'
        except json.JSONDecodeError:
            if len(text.splitlines()) > 1 and all(line.strip().startswith(('{', '[')) for line in text.splitlines() if line.strip()):
                return 'JSONL'
            return 'JSON'
    # Delimited text has no magic. Require multiple consistent records.
    for delimiter, fmt in (('\t', 'TSV'), (',', 'CSV')):
        try:
            sample = list(islice(csv.reader(io.StringIO(text[:65536]), delimiter=delimiter, strict=True), 20))
        except csv.Error:
            continue
        if len(sample) >= 2 and 1 < len(sample[0]) <= 256 and all(len(row) == len(sample[0]) for row in sample[:-1]):
            return fmt
    return 'UTF-8'


def typed_leaves(value: object, base: dict | None = None) -> list:
    units = []
    stack = [('', value, 0)]
    nodes = 0
    while stack:
        pointer, item, depth = stack.pop()
        nodes += 1
        if nodes > MAX_UNITS or depth > 64 or len(pointer) > 4096:
            raise ValueError('DOCUMENT_STRUCTURE_LIMIT：節點、深度或來源位置超限。')
        if isinstance(item, float) and not math.isfinite(item):
            raise ValueError('DOCUMENT_NUMBER：非有限數字。')
        if isinstance(item, dict) and item:
            if len(item) > MAX_UNITS:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：物件欄位超限。')
            stack.extend((pointer + '/' + str(k).replace('~', '~0').replace('/', '~1'), v, depth + 1)
                         for k, v in reversed(list(item.items())))
        elif isinstance(item, list) and item:
            if len(item) > MAX_UNITS:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：陣列超限。')
            stack.extend((pointer + '/' + str(i), item[i], depth + 1) for i in range(len(item)-1, -1, -1))
        else:
            kind = {str: 'string', int: 'integer', float: 'number', bool: 'boolean',
                    type(None): 'null', list: 'array', dict: 'object'}.get(type(item), 'datetime')
            rendered = json.dumps(item, ensure_ascii=False, allow_nan=False, default=str)
            units.append((dict(base or {}, pointer=pointer, value_type=kind), rendered))
    return units


def structured(data: bytes, fmt: str, options: dict) -> dict:
    text = text_source(data)
    units = []
    metadata = {}
    if fmt == 'JSON':
        units = typed_leaves(load_json(text))
    elif fmt == 'JSONL':
        for line, value in enumerate(text.splitlines(), 1):
            if not value.strip():
                continue
            units.extend(typed_leaves(load_json(value), {'line': line}))
            if len(units) > MAX_UNITS:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：JSONL 節點超限。')
    elif fmt in ('CSV', 'TSV'):
        csv.field_size_limit(128 * 1024)
        reader = csv.reader(io.StringIO(text, newline=''), delimiter=',' if fmt == 'CSV' else '\t', strict=True)
        header = next(reader, None)
        if not header or len(header) > 256 or any(len(c) > 256 for c in header):
            raise ValueError('DOCUMENT_COLUMNS：欄名或欄數超限。')
        metadata = dict(columns=header, column_types=['string'] * len(header), header_record=True)
        previous_line = reader.line_num
        for index, row in enumerate(reader, 1):
            if len(row) != len(header) or index > MAX_UNITS:
                raise ValueError('DOCUMENT_ROWS：欄數不一致或資料列超限。')
            units.append((dict(row=index, line_start=previous_line + 1, line_end=reader.line_num),
                          json.dumps(row, ensure_ascii=False)))
            previous_line = reader.line_num
    elif fmt == 'XML':
        tree = xml(data)
        stack = [(tree, '', 1)]
        while stack:
            node, parent, index = stack.pop()
            path = parent + '/' + node.tag + '[' + str(index) + ']'
            if len(path) > 4096 or len(units) >= MAX_UNITS:
                raise ValueError('DOCUMENT_STRUCTURE_LIMIT：XML 來源位置或節點超限。')
            units.append((dict(element=path), json.dumps(dict(tag=node.tag, attributes=node.attrib,
                          text=node.text, tail=node.tail), ensure_ascii=False)))
            stack.extend((child, path, i + 1) for i, child in reversed(list(enumerate(node))))
    elif fmt == 'TOML':
        if tomllib is None:
            raise ValueError('DOCUMENT_RUNTIME：TOML 需要 Python 3.11+ 的 tomllib。')
        units = typed_leaves(tomllib.loads(text))
    elif fmt == 'YAML':
        units = typed_leaves(yaml_value(text))
    elif fmt == 'INI':
        parser = configparser.ConfigParser(interpolation=None, strict=True)
        parser.optionxform = str
        parser.read_string(text)
        values = {'DEFAULT': dict(parser.defaults())}
        values.update({section: dict(parser[section]) for section in parser.sections()})
        units = typed_leaves(values)
        metadata['interpolation'] = False
    elif fmt in ('SRT', 'VTT'):
        normalized = text.replace('\r\n', '\n')
        blocks = re.split(r'\n[ \t]*\n', normalized.strip())
        if fmt == 'VTT':
            if not blocks or not re.fullmatch(r'WEBVTT(?:[ \t][^\n]*)?', blocks.pop(0)):
                raise ValueError('DOCUMENT_SUBTITLE：WebVTT header 無效。')

        def timestamp(value: str) -> float:
            pattern = r'(?:(\d{2,}):)?(\d{2}):(\d{2})[.,](\d{3})'
            match = re.fullmatch(pattern, value)
            if match is None or int(match[2]) > 59 or int(match[3]) > 59 or (fmt == 'SRT' and match[1] is None):
                raise ValueError('DOCUMENT_SUBTITLE：字幕時間戳無效。')
            return int(match[1] or 0)*3600 + int(match[2])*60 + int(match[3]) + int(match[4])/1000

        for index, block in enumerate(blocks):
            lines = block.splitlines()
            if fmt == 'VTT' and lines and (lines[0] == 'NOTE' or lines[0].startswith('NOTE ')):
                continue
            label = lines.pop(0) if lines and '-->' not in lines[0] else ''
            if fmt == 'SRT' and not label.isdigit():
                raise ValueError('DOCUMENT_SUBTITLE：SRT cue id 無效。')
            if not lines or index >= MAX_UNITS:
                raise ValueError('DOCUMENT_SUBTITLE：字幕 cue 無效或數量超限。')
            match = re.fullmatch(r'(\S+) --> (\S+)(?:[ \t]+([^\n]*))?', lines.pop(0))
            if match is None:
                raise ValueError('DOCUMENT_SUBTITLE：不支援的字幕區塊或時間格式。')
            start, stop = timestamp(match[1]), timestamp(match[2])
            if stop < start or len(label) > 256 or len(match[3] or '') > 256:
                raise ValueError('DOCUMENT_SUBTITLE：字幕時間或標籤超限。')
            units.append((dict(cue=index + 1, label=label, start_seconds=start, end_seconds=stop,
                               settings=match[3] or ''), '\n'.join(lines)))
    else:
        raise ValueError('DOCUMENT_FORMAT：不支援的結構化格式。')
    # Validate all syntax, dimensions and scalar types before returning any page.
    result = paginate(units, options['start'], options['limit'])
    return dict(format=fmt, unit='text segment (zero-based index)', **metadata, **result)


def sqlite_supported() -> bool:
    return hasattr(sqlite3.Connection, 'deserialize') and hasattr(sqlite3.Connection, 'setlimit')


def sqlite_document(data: bytes, options: dict) -> dict:
    if not sqlite_supported():
        raise ValueError('DOCUMENT_RUNTIME：SQLite 需要具備 deserialize/setlimit 的 Python 3.11+。')
    if len(data) < 100 or data[18:20] != b'\x01\x01':
        raise ValueError('DOCUMENT_SQLITE：拒絕 WAL 或無效 SQLite 快照；需獨立完整資料庫。')
    connection = sqlite3.connect(':memory:', cached_statements=0)
    try:
        connection.setlimit(sqlite3.SQLITE_LIMIT_LENGTH, 1024 * 1024)
        connection.setlimit(sqlite3.SQLITE_LIMIT_COLUMN, 256)
        connection.setlimit(sqlite3.SQLITE_LIMIT_SQL_LENGTH, 65536)
        connection.setlimit(sqlite3.SQLITE_LIMIT_EXPR_DEPTH, 64)
        connection.setlimit(sqlite3.SQLITE_LIMIT_ATTACHED, 0)
        connection.deserialize(data)
        connection.execute('PRAGMA trusted_schema=OFF')
        connection.execute('PRAGMA temp_store=MEMORY')
        connection.execute('PRAGMA query_only=ON')
        steps = 0

        def progress() -> int:
            nonlocal steps
            steps += 1
            return int(steps > 20000)

        connection.set_progress_handler(progress, 1000)

        def authorize(action, arg1, arg2, database, trigger):
            if trigger is not None:
                return sqlite3.SQLITE_DENY
            if action in (sqlite3.SQLITE_SELECT, sqlite3.SQLITE_READ):
                return sqlite3.SQLITE_OK
            if action == sqlite3.SQLITE_PRAGMA and arg1 in ('table_list', 'table_xinfo'):
                return sqlite3.SQLITE_OK
            return sqlite3.SQLITE_DENY

        connection.set_authorizer(authorize)
        # Do not run quick_check/integrity_check: those can evaluate CHECK
        # expressions stored in the source schema. SELECT only ordinary columns.
        catalog = connection.execute('PRAGMA table_list').fetchmany(1025)
        if len(catalog) > 1024:
            raise ValueError('DOCUMENT_SQLITE_LIMIT：資料表數超限。')
        tables = sorted(row[1] for row in catalog if row[0] == 'main' and row[2] == 'table' and not row[1].startswith('sqlite_'))
        requested = options.get('table')
        units = []
        columns = []
        for name in tables:
            if len(name) > 256:
                raise ValueError('DOCUMENT_SQLITE_LIMIT：資料表名稱超限。')
            if requested is not None and name != requested:
                continue
            quoted = '"' + name.replace('"', '""') + '"'
            info = connection.execute('PRAGMA table_xinfo(' + quoted + ')').fetchall()
            # Generated columns can execute expressions. Exclude the whole table.
            if any(row[6] for row in info):
                if requested == name:
                    raise ValueError('DOCUMENT_SQLITE：不讀取 generated/hidden 欄位資料表。')
                continue
            columns = [dict(name=row[1], declared_type=row[2], primary_key=row[5]) for row in info]
            if any(len(row['name']) > 256 or len(row['declared_type']) > 256 for row in columns):
                raise ValueError('DOCUMENT_SQLITE_LIMIT：欄位 metadata 超限。')
            if requested is None:
                units.append((dict(table=name), json.dumps(columns, ensure_ascii=False)))
                continue
            # Identifiers come only from the validated schema and are quoted; no SQL input.
            selected = ','.join('"' + row['name'].replace('"', '""') + '"' for row in columns)
            for index, row in enumerate(connection.execute('SELECT ' + selected + ' FROM ' + quoted + ' LIMIT 100001')):
                if index >= MAX_UNITS:
                    raise ValueError('DOCUMENT_SQLITE_LIMIT：資料列超限。')
                values = []
                for value in row:
                    if isinstance(value, bytes):
                        value = dict(type='blob', encoding='base64', data=base64.b64encode(value).decode('ascii'))
                    if isinstance(value, float) and not math.isfinite(value):
                        raise ValueError('DOCUMENT_NUMBER：非有限 SQLite 數字。')
                    values.append(value)
                units.append((dict(table=name, row=index), json.dumps(values, ensure_ascii=False, allow_nan=False)))
        if requested is not None and requested not in tables:
            raise ValueError('DOCUMENT_SQLITE：不存在可讀取的一般資料表。')
        return dict(format='SQLITE', columns=columns if requested else None,
                    mode='rows' if requested else 'table schema', order='snapshot storage order',
                    unit='text segment (zero-based index)', **paginate(units, options['start'], options['limit']))
    finally:
        connection.close()
