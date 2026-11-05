"""回歸環境不符時應先拒絕，不修改服務的正式路徑或 token 防護。"""
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tools.verification import verify_isolated


class VerificationEnvironmentTests(unittest.TestCase):
    def test_rejected_token_never_creates_probe(self):
        with patch.object(verify_isolated, 'verify_user_token', side_effect=ValueError('synthetic token')), \
             patch.object(verify_isolated.tempfile, 'TemporaryDirectory') as create:
            with self.assertRaisesRegex(RuntimeError, 'TEST_TOKEN_UNSUITABLE'):
                verify_isolated.check_test_environment()
            create.assert_not_called()

    def test_protected_temp_fails_before_any_write(self):
        root = Path.cwd()
        with patch.object(verify_isolated, 'verify_user_token'), \
             patch.object(verify_isolated.tempfile, 'gettempdir', return_value=str(root / '.test-tmp')), \
             patch.object(verify_isolated.tempfile, 'TemporaryDirectory') as create:
            with self.assertRaisesRegex(RuntimeError, 'TEST_TEMP_PROTECTED'):
                verify_isolated.check_test_environment()
            create.assert_not_called()
        with patch.object(verify_isolated, 'verify_user_token'), \
             patch('mcp_local.security_policy.python_runtime_paths', return_value=(root / 'synthetic-python',)), \
             patch('mcp_local.security_policy.STATE_DIR', root / 'synthetic-state'), \
             patch.object(verify_isolated.tempfile, 'gettempdir', return_value=str(root / 'synthetic-state/temp')), \
             patch.object(verify_isolated.tempfile, 'TemporaryDirectory') as create:
            with self.assertRaisesRegex(RuntimeError, 'TEST_TEMP_PROTECTED'):
                verify_isolated.check_test_environment()
            create.assert_not_called()

    def test_probe_uses_only_owned_temp_and_cleans_it(self):
        with tempfile.TemporaryDirectory(prefix='mcp-test-environment-') as directory:
            root = Path(directory)
            temporary = root / 'temporary'
            temporary.mkdir()
            with patch.object(verify_isolated, 'verify_user_token'), \
                 patch('mcp_local.security_policy.python_runtime_paths', return_value=(root / 'python',)), \
                 patch('mcp_local.security_policy.STATE_DIR', root / 'state'), \
                 patch.object(verify_isolated.tempfile, 'gettempdir', return_value=str(temporary)):
                result = verify_isolated.check_test_environment()
                self.assertEqual(result['nested_write_hardlink_cleanup'], 'PASS')
                self.assertEqual(list(temporary.iterdir()), [])
                with patch.object(verify_isolated.os, 'link', side_effect=PermissionError('synthetic denied')):
                    with self.assertRaisesRegex(RuntimeError, 'TEST_TEMP_UNUSABLE'):
                        verify_isolated.check_test_environment()
                self.assertEqual(list(temporary.iterdir()), [])


if __name__ == '__main__':
    unittest.main()
