"""資料夾選取失敗時保留可操作的原因，且不修改既有授權。"""
from pathlib import Path
import unittest
from unittest.mock import patch

from workspace_settings import create_workspace_root


class WorkspaceRootErrorTests(unittest.TestCase):
    def test_drive_root_allows_hidden_root_but_rejects_hidden_children(self):
        from local_files_mcp import FileReader
        with patch('local_files_mcp.hidden', return_value=True) as hidden:
            row = create_workspace_root(Path.cwd().anchor, [])
            reader = FileReader(Path(row['path']))
            self.assertEqual(reader.checked('.'), Path(Path.cwd().anchor))
            hidden.assert_not_called()
            with self.assertRaisesRegex(ValueError, '隱藏'):
                reader.checked(Path.cwd().relative_to(reader.root).as_posix())

    def test_drive_root_roundtrip_and_read_with_state_exclusion(self):
        import os
        import tempfile
        from local_files_mcp import FileReader
        from workspace_settings import encode_workspace, decode_workspace
        with tempfile.TemporaryDirectory(dir=Path.cwd()) as directory:
            folder = Path(directory).resolve()
            public = folder / 'public.txt'
            public.write_text('test', encoding='utf-8')
            state = folder / 'state'
            state.mkdir()
            (state / 'key.txt').write_text('private', encoding='utf-8')
            with patch('security_policy.STATE_DIR', state), patch('connection_settings.CONFIG_DIR', state):
                row = create_workspace_root(folder.anchor, [])
                workspace = {'roots': [row], 'default_root': row['id']}
                self.assertEqual(decode_workspace(encode_workspace(workspace)), workspace)
                reader = FileReader(Path(folder.anchor))
                self.assertEqual(reader.checked(public.relative_to(reader.root).as_posix()), public)
                for path in (state, state / 'key.txt'):
                    with self.assertRaisesRegex(ValueError, '金鑰'):
                        reader.checked(path.relative_to(reader.root).as_posix())
                with os.scandir(folder) as entries:
                    for entry in entries:
                        if entry.name == 'state':
                            with self.assertRaisesRegex(ValueError, '金鑰'):
                                reader._entry_info(folder, entry)
                with self.assertRaises(ValueError):
                    FileReader(state)
                with self.assertRaises(ValueError):
                    FileReader(folder)

    def test_validation_reason_is_preserved_without_mutating_roots(self):
        previous = {'id': 'old', 'name': 'old', 'path': str(Path.cwd()),
                    'excluded_names': ['private']}
        roots = [previous.copy()]
        reasons = (
            '指定的共享資料夾不存在。請先建立資料夾。',
            '共享資料夾不可具有隱藏屬性或點號名稱。',
            '共享資料夾不可為符號連結或重新解析點。',
            '共享範圍不可與程式設定、備份及金鑰目錄重疊。',
            '不可共享整個使用者家目錄。請指定專用資料夾。',
        )
        for reason in reasons:
            with self.subTest(reason=reason):
                with patch('local_files_mcp.FileReader', side_effect=ValueError(reason)):
                    with self.assertRaises(ValueError) as caught:
                        create_workspace_root(str(Path.cwd()), roots, previous=previous)
                self.assertIn(reason, str(caught.exception))
                self.assertEqual(roots, [previous])

    def test_filesystem_errors_have_safe_actionable_messages(self):
        for error, expected in (
            (PermissionError('sensitive detail'), '沒有權限'),
            (FileNotFoundError('sensitive detail'), '已不存在'),
            (OSError('sensitive detail'), '磁碟或網路位置'),
        ):
            with self.subTest(error=type(error).__name__):
                with patch('local_files_mcp.FileReader', side_effect=error):
                    with self.assertRaises(ValueError) as caught:
                        create_workspace_root(str(Path.cwd()), [])
                self.assertIn(expected, str(caught.exception))
                self.assertNotIn('sensitive detail', str(caught.exception))
