"""監控快照的唯讀展示與匯出；不修改擷取容量或原始資料。"""
from __future__ import annotations
from itertools import islice
import json
from typing import Any
from mcp_local.request_monitor import Monitor

def display_json(value: Any, *, pretty: bool = True) -> str:
    """Escape lone surrogates for display without changing the captured object."""
    return json.dumps(value, ensure_ascii=False, indent=2 if pretty else None).encode(
        'utf-8', 'backslashreplace').decode('utf-8')

def preview(value: Any, limit: int = 100) -> str:
    if isinstance(value, dict):
        return f'{{…}} {len(value)} 個欄位'
    if isinstance(value, list):
        return f'[…] {len(value)} 個項目'
    # Slice before encoding: a scalar may contain the whole capture budget.
    text = display_json(value[:limit] if isinstance(value, str) else value, pretty=False)
    return text[:limit] + ('…' if len(text) > limit or isinstance(value, str) and len(value) > limit else '')

def argument_summary(row: dict) -> str:
    request = row.get('request')
    params = request.get('params') if isinstance(request, dict) else None
    if not isinstance(params, dict) or 'arguments' not in params:
        return '未擷取參數'
    arguments = params['arguments']
    if not isinstance(arguments, dict):
        return preview(arguments, 140)
    parts = [f'{preview(key, 35)}: {preview(value, 70)}'
             for key, value in islice(arguments.items(), 3)]
    text = ', '.join(parts) if parts else '{}'
    return text[:180] + ('…' if len(text) > 180 or len(arguments) > 3 else '')

def contains(value: Any, query: str) -> bool:
    """Search decoded keys/values, including paths, without caching raw payloads."""
    if isinstance(value, dict):
        return any(query in str(key).casefold() or contains(child, query)
                   for key, child in value.items())
    if isinstance(value, list):
        return any(contains(child, query) for child in value)
    if isinstance(value, str):
        return query in value.casefold()
    return query in json.dumps(value).casefold()

def matches(row: dict, query: str, tool: str, status: str) -> bool:
    if tool and row.get('tool') != tool or status and row.get('status') != status:
        return False
    return not query or any(contains(row.get(key), query) for key in
                            ('tool', 'request', 'response', 'request_notes', 'response_notes'))

def record_document(row: dict, monitor: Monitor) -> dict:
    """Export only the selected capture, with all original diagnostic notes."""
    return {
        'format': 'mcp-local-request-monitor-v1',
        'record': {key: value for key, value in row.items() if not key.startswith('_')},
        'diagnostics': {
            'lost_events': monitor.lost_events,
            'evicted_records': monitor.evicted_records,
            'dropped_total': monitor.dropped,
            'scope': '目前監控歷程的累計計數，非此紀錄專屬；事件遺失僅計入可偵測的佇列覆寫。',
            'capture': '僅含記憶體中已擷取的內容；省略、截斷與未收到的內容無法還原。',
        },
    }
