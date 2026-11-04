"""真實 Python 私有管線／Job／DPAPI；通道、排程、電源僅隔離合成替身。"""
from pathlib import Path
import json
import os
import queue
import subprocess
import sys
import tempfile
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix='mcp-wpf-bridge-')
        self.addCleanup(self.directory.cleanup)
        self.process = subprocess.Popen([sys.executable, '-B', '-X', 'faulthandler', '-m', 'mcp_local.desktop_bridge',
            '--fixture', self.directory.name], cwd=ROOT, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.PIPE, creationflags=subprocess.CREATE_NO_WINDOW)
        self.addCleanup(self.close)
        self.events = queue.Queue()
        self.sequence = 0
        self.notifications = []
        def read():
            for line in self.process.stdout:
                self.events.put(json.loads(line))
        threading.Thread(target=read, daemon=True).start()

    def close(self):
        if self.process.stdin and not self.process.stdin.closed:
            self.process.stdin.close()
        try:
            self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
            self.fail('Fixture backend did not exit after private pipe EOF')
        for stream in (self.process.stdout, self.process.stderr):
            stream.close()

    def call(self, command, args=None, *, confirm=False):
        self.sequence += 1
        identifier = self.sequence
        self.process.stdin.write((json.dumps({'id': identifier, 'command': command, 'args': args or {}}) + '\n').encode())
        self.process.stdin.flush()
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            try:
                message = self.events.get(timeout=.1)
            except queue.Empty:
                if self.process.poll() is not None:
                    self.fail('backend exited: ' + self.process.stderr.read().decode(errors='replace'))
                continue
            if message.get('event'):
                self.notifications.append(message)
                if message['event'] == 'dialog' and message['kind'] == 'confirm':
                    self.process.stdin.write((json.dumps({'id': identifier, 'command': 'dialog_answer',
                        'token': message['token'], 'accepted': confirm}) + '\n').encode())
                    self.process.stdin.flush()
                continue
            if message.get('id') == identifier:
                return message
        self.fail('bridge command timeout: ' + command)

    def result(self, command, args=None, **kwargs):
        value = self.call(command, args, **kwargs)
        self.assertNotIn('error', value, value)
        return value['result']

    def wait_state(self, predicate):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            state = self.result('state')
            if predicate(state):
                return state
            time.sleep(.03)
        self.fail('state did not settle: ' + str(state) + '\nnotifications: ' + str(self.notifications))

    def test_protocol_form_dpapi_transaction_monitor_and_shutdown(self):
        hello = self.result('hello')
        self.assertEqual(hello['protocol'], 1)
        self.assertEqual(len(hello['modes']), 3)
        self.assertNotIn('synthetic-fixture-key', json.dumps(hello))
        self.assertTrue(hello['state']['key_available'])
        self.assertNotIn('tkinter', hello)
        self.result('save', {'form': {'tunnel': 'tunnel_changed', 'key': 'synthetic-new-key'}})
        saved = self.wait_state(lambda s: not s['busy'] and not s['dirty'])
        self.assertEqual(saved['form']['tunnel'], 'tunnel_changed')
        raw = (Path(self.directory.name) / 'settings.json').read_text(encoding='utf-8')
        self.assertNotIn('synthetic-new-key', raw)
        self.assertNotIn(b'synthetic-new-key', (Path(self.directory.name) / 'fixture-key.dpapi').read_bytes())
        self.result('start')
        self.wait_state(lambda s: s['state'] == 'RUNNING')
        deadline = time.monotonic() + 8
        monitor = None
        while time.monotonic() < deadline:
            monitor = self.result('monitor')
            if monitor['records']:
                break
            time.sleep(.05)
        self.assertEqual(monitor['records'][0]['id'], 'fixture-call')
        self.assertNotIn('response', monitor['records'][0])
        detail = self.result('detail', {'record_id': 'fixture-call'})['document']
        self.assertEqual(detail['format'], 'mcp-local-request-monitor-v1')
        self.assertEqual(detail['record']['response']['text'], '合成整合資料')
        self.assertEqual(detail['record']['response_notes'][0]['retained_chars'], 6)
        self.assertEqual(self.result('monitor_search', {'query': '合成整合'})['ids'], ['fixture-call'])
        self.result('toggle_request_monitor')
        self.assertFalse(self.result('monitor')['enabled'])
        self.result('stop')
        self.wait_state(lambda s: not s['running'] and s['state'] == 'IDLE')
        self.result('quit')
        self.process.wait(timeout=8)
        self.assertEqual(self.process.returncode, 0, self.process.stderr.read().decode(errors='replace'))

    def test_permissions_confirmation_and_unsupported_operations_fail_closed(self):
        self.result('hello')
        initial = (Path(self.directory.name) / 'settings.json').read_bytes()
        self.result('save', {'form': {'access_mode': 'host_control'}}, confirm=False)
        self.assertEqual((Path(self.directory.name) / 'settings.json').read_bytes(), initial)
        self.assertTrue(any(n.get('kind') == 'confirm' for n in self.notifications))
        self.assertIn('error', self.call('execute', {'command': 'anything'}))
        self.assertIn('error', self.call('edit', {'form': {'unknown': 'value'}}))
        self.assertIn('error', self.call('edit', {'form': {'tunnel': 'changed', 'numbers': {'unknown': '0'}}}))
        self.assertEqual(self.result('state')['form']['tunnel'], 'tunnel_fixture')
        self.result('save', {'form': {'access_mode': 'host_control'}}, confirm=True)
        saved = self.wait_state(lambda s: not s['busy'] and not s['dirty'])
        self.assertEqual(saved['form']['access_mode'], 'host_control')

    def test_pipe_eof_stops_owned_running_fixture(self):
        self.result('hello')
        self.result('start')
        self.wait_state(lambda s: s['state'] == 'RUNNING')
        self.process.stdin.close()
        self.process.wait(timeout=10)
        self.assertEqual(self.process.returncode, 0)

    def test_malformed_frame_terminates_without_actions(self):
        self.result('hello')
        initial = (Path(self.directory.name) / 'settings.json').read_bytes()
        self.process.stdin.write(b'[]\n')
        self.process.stdin.flush()
        self.process.wait(timeout=10)
        self.assertEqual((Path(self.directory.name) / 'settings.json').read_bytes(), initial)


if __name__ == '__main__':
    unittest.main()
