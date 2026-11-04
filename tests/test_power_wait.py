"""Native waits do not change Windows power policy or open a tunnel."""
import subprocess
import sys
import threading
import time
import unittest
from mcp_local.power_windows import CancelEvent


class NativeWaitTests(unittest.TestCase):
    def test_process_exit_and_timeout(self):
        event = CancelEvent()
        process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(.3)'], creationflags=0x08000000)
        try:
            self.assertFalse(event.wait_process(process, .01))
            self.assertTrue(event.wait_process(process, 3))
            self.assertEqual(process.returncode, 0)
        finally:
            process.kill() if process.poll() is None else None
            process.wait()
            event.close()

    def test_cancel_wakes_blocked_wait(self):
        event = CancelEvent()
        process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'], creationflags=0x08000000)
        worker = threading.Timer(.1, event.set)
        try:
            worker.start()
            before = time.monotonic()
            self.assertTrue(event.wait_process(process))
            self.assertLess(time.monotonic() - before, 2)
            self.assertTrue(event.is_set())
        finally:
            worker.join()
            process.kill()
            process.wait()
            event.close()

