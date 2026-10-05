"""舊控制物件與已入列寫入在服務關閉後不得繼續修改 fixture。"""
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch

from full_control import FullControl
from workspace_reader import WorkspaceReader


class ControlRevocationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-revocation-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        workspace = WorkspaceReader([{'id': 'main', 'path': str(self.root)}], 'main')
        self.control = FullControl(workspace)
        self.addCleanup(self.control.close)

    def test_closed_controller_refuses_all_file_mutations(self):
        saved = self.control.write_file('existing.txt', 'original')
        self.control.close()
        calls = [
            lambda: self.control.write_file('new.txt', 'new'),
            lambda: self.control.write_file('existing.txt', 'changed', 'rewrite', saved['sha256']),
            lambda: self.control.edit_block('existing.txt', 'original', 'changed', saved['sha256']),
            lambda: self.control.create_directory('new-directory'),
            lambda: self.control.move_file('existing.txt', 'moved.txt', saved['sha256']),
            lambda: self.control.delete_file('existing.txt', saved['sha256']),
        ]
        for index, call in enumerate(calls):
            with self.subTest(operation=index), self.assertRaisesRegex(ValueError, 'CONTROL_CLOSED'):
                call()
        self.assertEqual((self.root / 'existing.txt').read_text(), 'original')
        self.assertEqual({path.name for path in self.root.iterdir()}, {'existing.txt'})
        with self.assertRaisesRegex(ValueError, 'SESSION_CLOSED'):
            self.control.list_sessions()

    def test_write_admitted_before_close_cannot_commit_after_revocation(self):
        admitted, resume = threading.Event(), threading.Event()
        original_reader = self.control.files.reader
        outcomes = []

        def delayed_reader(root_id):
            reader = original_reader(root_id)
            admitted.set()
            if not resume.wait(10):
                raise RuntimeError('test synchronization timeout')
            return reader

        def write():
            try:
                self.control.write_file('queued.txt', 'must not be written')
            except Exception as error:
                outcomes.append(error)
            else:
                outcomes.append('unexpected write')

        with patch.object(self.control.files, 'reader', side_effect=delayed_reader):
            worker = threading.Thread(target=write, daemon=True)
            worker.start()
            try:
                self.assertTrue(admitted.wait(5))
                self.control.close()
            finally:
                resume.set()
                worker.join(10)
        self.assertFalse(worker.is_alive())
        self.assertEqual(len(outcomes), 1)
        self.assertIsInstance(outcomes[0], ValueError)
        self.assertIn('CONTROL_CLOSED', str(outcomes[0]))
        self.assertFalse((self.root / 'queued.txt').exists())

    def test_cleanup_failure_does_not_restore_file_authority(self):
        with patch.object(self.control.sessions, 'close', side_effect=ValueError('fixture cleanup failure')):
            with self.assertRaisesRegex(ValueError, 'fixture cleanup failure'):
                self.control.close()
        with self.assertRaisesRegex(ValueError, 'CONTROL_CLOSED'):
            self.control.write_file('after-failure.txt', 'must not be written')
        self.assertFalse((self.root / 'after-failure.txt').exists())


if __name__ == '__main__':
    unittest.main()
