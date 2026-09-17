"""Native waits do not change Windows power policy or open a tunnel."""
import subprocess
import sys
import threading
import time
import unittest
from power_windows import CancelEvent


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

    def test_live_mode_channel_updates_existing_child_without_restart(self):
        import os
        import queue
        from power_windows import WindowsPower, ModeChannel
        api = WindowsPower()
        channel = ModeChannel(api, 'off')
        program = ('import sys,threading; from power_windows import WindowsPower,follow_mode; '
                   'WindowsPower.qos=lambda self,enabled: print(int(enabled),flush=True); '
                   'follow_mode(sys.argv[1],int(sys.argv[2])); threading.Event().wait()')
        process = subprocess.Popen([sys.executable, '-B', '-c', program, channel.token, str(os.getpid())],
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, creationflags=0x08000000)
        messages = queue.Queue()
        def drain():
            for line in process.stdout:
                messages.put(line.strip())
        worker = threading.Thread(target=drain, daemon=True)
        worker.start()
        try:
            self.assertEqual(messages.get(timeout=5), '0')
            for mode, expected in [('extreme', '1'), ('off', '0')]:
                channel.publish(mode)
                self.assertEqual(messages.get(timeout=5), expected)
                self.assertIsNone(process.poll())
        finally:
            process.kill()
            process.wait()
            worker.join(timeout=2)
            process.stdout.close()
            process.stderr.close()
            channel.close()
