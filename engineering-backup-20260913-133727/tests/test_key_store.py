"""只使用假金鑰，驗證 Windows 加密保存與跨程序還原。"""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from key_store import KeyStore


class KeyStoreTests(unittest.TestCase):
    def test_encrypted_roundtrip_and_next_process(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'api-key.dpapi'
            store = KeyStore(path)
            self.assertEqual(store.load(), '')
            store.save('fake-key-for-local-test')
            self.assertNotIn(b'fake-key-for-local-test', path.read_bytes())
            self.assertEqual(store.load(), 'fake-key-for-local-test')
            result = subprocess.run([sys.executable, '-B', '-c',
                'from pathlib import Path; from key_store import KeyStore; import sys; '
                'assert KeyStore(Path(sys.argv[1])).load() == "fake-key-for-local-test"', str(path)],
                capture_output=True, timeout=10, creationflags=subprocess.CREATE_NO_WINDOW)
            self.assertEqual(result.returncode, 0)
            store.save('')
            self.assertEqual(store.load(), 'fake-key-for-local-test')
            store.save('fake-replacement')
            self.assertEqual(store.load(), 'fake-replacement')

    def test_corrupt_store_is_not_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'api-key.dpapi'
            path.write_bytes(b'corrupt-data')
            with self.assertRaises(OSError):
                KeyStore(path).load()
            self.assertEqual(path.read_bytes(), b'corrupt-data')
