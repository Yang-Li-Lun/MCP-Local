"""命令組裝用的非執行成品；保留真實 SHA-256 驗證，不依賴本機 vendor。"""
from contextlib import contextmanager
import hashlib
import json
from pathlib import Path
import tempfile
from typing import Iterator
from unittest.mock import patch


@contextmanager
def command_project() -> Iterator[Path]:
    with tempfile.TemporaryDirectory(prefix='mcp-command-') as directory:
        project = Path(directory)
        data = b'non-executable command fixture'
        (project / 'tunnel-client.exe').write_bytes(data)
        (project / 'vendor-integrity.json').write_text(json.dumps({
            'tunnel-client.exe': {'sha256': hashlib.sha256(data).hexdigest()}}), encoding='utf-8')
        python = project / '.venv' / 'Scripts' / 'python.exe'
        python.parent.mkdir(parents=True)
        python.write_bytes(b'non-executable fixture')
        (project / 'local_files_mcp.py').write_text('# fixture; never executed\n', encoding='utf-8')
        with patch('connection_settings.PROJECT', project):
            yield project
