import asyncio
import unittest
from verify_full_control_stdio import verify


class FullControlStdioTests(unittest.TestCase):
    def test_real_stdio_switches_and_calls(self):
        result = asyncio.run(verify())
        self.assertEqual([mode['tools'] for mode in result['modes']], [22, 34, 22])
        self.assertGreater(result['calls'], 50)
