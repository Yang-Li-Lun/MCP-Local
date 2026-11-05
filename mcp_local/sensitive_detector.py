"""有界敏感文字分類；固定線性範圍規則與 literal，不執行使用者正規表示式。"""
from __future__ import annotations

import re

PATTERNS = {
    'private_key': re.compile(r'-----BEGIN (?:[A-Z0-9 ]{0,30})PRIVATE KEY-----'),
    'api_key': re.compile(r'(?i)(?:\b(?:sk|ghp|github_pat|xox[baprs])[-_][a-z0-9_-]{8,}|\bAKIA[A-Z0-9]{16}\b)'),
    'bearer': re.compile(r'(?i)\bBearer\s+[a-z0-9._~+/=-]{8,}'),
    'password': re.compile(r'''(?i)["']?(?:password|passwd|secret|api[_-]?key|access[_-]?token)["']?\s*[:=]\s*(?:"[^"\r\n]{3,}"|'[^'\r\n]{3,}'|[^\s,"';}\]]{3,})'''),
    'email': re.compile(r'(?i)\b[a-z0-9._%+-]{1,64}@[a-z0-9.-]{1,190}\.[a-z]{2,20}\b'),
}
SYNTHETIC = 'password=synthetic_password_123\nBearer synthetic_token_123456\nsk-synthetic_1234567890\nexample@example.invalid'
MAX_CHARS = 4 * 1024 * 1024
PRIVATE_END = re.compile(r'-----END (?:[A-Z0-9 ]{0,30})PRIVATE KEY-----')


class SensitiveDetector:
    def __init__(self, policy: dict):
        self.enabled = policy['detector_enabled']
        self.rules = [(rule, PATTERNS[rule['pattern']] if rule['kind'] == 'builtin' else re.compile(re.escape(rule['pattern'])))
                      for rule in policy['rules'] if rule['enabled']]

    def inspect(self, text: str) -> tuple[str, list[dict]]:
        if len(text) > MAX_CHARS:
            raise ValueError('SECURITY_SCAN_LIMIT')
        if not self.enabled:
            return text, []
        spans, findings = [], []
        for rule, pattern in self.rules:
            count = 0
            private_until = 0
            for match in pattern.finditer(text):
                span = match.span()
                if rule['kind'] == 'builtin' and rule['pattern'] == 'private_key':
                    if match.start() < private_until:
                        continue
                    end = PRIVATE_END.search(text, match.end())
                    private_until = end.end() if end else len(text)
                    span = (match.start(), private_until)
                spans.append(span)
                count += 1
                if len(spans) > 10000:
                    raise ValueError('SECURITY_MATCH_LIMIT')
            if count:
                findings.append(dict(rule=rule['id'], risk=rule['risk'], count=count))
        merged = []
        for start, end in sorted(spans):
            if merged and start <= merged[-1][1]:
                merged[-1] = (merged[-1][0], max(end, merged[-1][1]))
            else:
                merged.append((start, end))
        # Same-length masking preserves session output offsets across pages.
        chunks, position = [], 0
        for start, end in merged:
            chunks.extend((text[position:start], '█' * (end - start)))
            position = end
        chunks.append(text[position:])
        return ''.join(chunks), findings
