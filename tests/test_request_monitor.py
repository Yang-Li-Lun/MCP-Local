"""詳細監控：合成 JSON、原生記憶體 IPC、隔離 GUI／真實 STDIO，無正式服務。"""
import asyncio
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from mcp_local.desktop_ports import Scheduler
import unittest
from unittest.mock import Mock, patch
from uuid import uuid4

from mcp import types
from mcp.client.stdio import get_default_environment
from mcp.shared.message import SessionMessage
from mcp.server.fastmcp import FastMCP
from mcp_local import request_monitor as monitor
from mcp_local.tool_contract import verify_contract

PROJECT = Path(__file__).resolve().parents[1]
BOOTSTRAP = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from mcp_local import security_policy, read_call_trace, developer_control, developer_toolchains
security_policy.STATE_DIR = Path(sys.argv[2]).parent / (Path(sys.argv[2]).name + '-state')
read_call_trace.PROJECT = Path(sys.argv[2]) / 'trace-disabled'
developer_control.backend_status = lambda toolchains=None: {'ready': False, 'reason': 'FIXTURE', 'host_fallback': False}
developer_toolchains.discover_toolchains = lambda roots=None: []
from mcp_local.local_files_mcp import main
sys.argv = ['local_files_mcp.py', '--root', sys.argv[2], '--access-mode', sys.argv[3]]
main()
'''


def message(value):
    return SessionMessage(types.JSONRPCMessage.model_validate(value))


def request(identifier=1, arguments=None, **extras):
    return dict(jsonrpc='2.0', id=identifier, method='tools/call',
                params=dict(name='read_file', arguments=arguments or {}), **extras)


def response(identifier=1, result=None):
    return dict(jsonrpc='2.0', id=identifier,
                result=result if result is not None else {'content': [], 'isError': False})


def event(identifier, **kwargs):
    return dict(id=identifier, time='2026-11-01T12:34:56.123+08:00',
                tool='read_file', status='running', request={}, **kwargs)


@unittest.skipUnless(os.name == 'nt', 'Windows named memory')
class RequestMonitorTests(unittest.TestCase):
    def setUp(self):
        self.model = monitor.Monitor()
        self.addCleanup(self.model.close)

    def connect(self):
        self.model.begin_connection()
        self.assertFalse(self.model.failed)
        client = monitor.MemoryChannel(self.model.channel.token)
        self.addCleanup(client.close)
        return client

    def drain(self):
        # Production drains at most four events per GUI tick.
        for _ in range(monitor.SLOTS // 4 + 1):
            self.model.poll()

    def test_connection_auto_enable_pause_resume_disconnect_and_new_generation(self):
        self.assertFalse(self.model.connected)
        self.assertFalse(self.model.enabled)
        self.assertEqual(self.model.environment(), {})
        client = self.connect()
        observer = monitor.Observer(client)
        observer.receive(message(request(arguments={'path': 'SYNTHETIC_PRIVATE'})))
        self.drain()
        self.assertTrue(self.model.enabled)
        self.assertEqual(next(iter(self.model.records.values()))['request']['params']['arguments']['path'],
                         'SYNTHETIC_PRIVATE')
        self.model.set_enabled(False)
        with patch.object(monitor, 'capture_message', side_effect=AssertionError('paused')) as capture:
            observer.receive(message(request(2)))
            observer.send(message(response()))
            capture.assert_not_called()
        self.assertEqual(client.epoch(), 0)
        self.model.set_enabled(True)
        observer.send(message(response()))
        self.drain()
        self.assertEqual(next(iter(self.model.records.values()))['status'], 'paused')
        observer.receive(message(request(3)))
        self.model.clear()
        observer.send(message(response(3)))
        self.drain()
        self.assertFalse(self.model.records)
        old_token = self.model.channel.token
        self.model.disconnect()
        self.assertEqual(client.epoch(), 0)
        self.model.poll()
        self.assertFalse(self.model.connected)
        self.model.set_enabled(True)
        self.assertFalse(self.model.enabled)
        self.connect()
        self.assertTrue(self.model.enabled)
        self.assertNotEqual(self.model.channel.token, old_token)
        self.assertEqual(client.epoch(), 0)

    def test_capture_preserves_arbitrary_json_text_errors_null_and_extra_fields(self):
        arguments = {'path': 'C:\\private\\資料.txt', 'query': 'Bearer SYNTHETIC_TOKEN',
                     'command': 'Write-Output secret', 'password': 'SYNTHETIC_PASSWORD',
                     'stdout': 'line one\nline two', 'stderr': 'error details',
                     'unknown': [None, True, 1, -1.25, {'custom': ''}], 'type': 'image', 'data': 'plain text'}
        original = request('client-id', arguments, custom=None, _meta={'debug': 'raw'})
        captured, notes = monitor.capture_message(message(original).message.root, incoming=True)
        self.assertEqual(captured, original)
        self.assertEqual(notes, [])
        null = dict(jsonrpc='2.0', id=5, method='tools/call', params=None)
        self.assertEqual(monitor.capture_message(message(null).message.root, incoming=True), (null, []))
        absent = dict(jsonrpc='2.0', id=6, method='tools/call')
        self.assertEqual(monitor.capture_message(message(absent).message.root, incoming=True), (absent, []))
        result = response('client-id', {'content': [{'type': 'text', 'text': json.dumps(arguments)}],
                                       'structuredContent': arguments, 'isError': True})
        captured, notes = monitor.capture_message(message(result).message.root)
        self.assertEqual(captured, result)
        self.assertIsInstance(captured['result']['content'][0]['text'], str)
        self.assertEqual(notes, [])
        error = dict(jsonrpc='2.0', id=9, error={'code': -32602, 'message': 'SYNTHETIC_ERROR',
                                               'data': {'path': 'C:/private', 'null': None}})
        self.assertEqual(monitor.capture_message(message(error).message.root), (error, []))

    def test_large_binary_and_deep_payload_are_bounded_and_explicit(self):
        original = response(result={'content': [
            {'type': 'image', 'data': 'YQ==', 'mimeType': 'image/png'},
            {'type': 'audio', 'data': 'YWI=', 'mimeType': 'audio/wav'},
            {'type': 'resource', 'resource': {'uri': 'file:///private', 'blob': 'YWJj', 'mimeType': 'application/octet-stream'}},
            {'type': 'text', 'text': '中文\n' * monitor.MAX_CAPTURE_BYTES},
        ], 'isError': False})
        before = original['result']['content'][3]['text']
        captured, notes = monitor.capture_message(message(original).message.root)
        self.assertEqual([note['decoded_bytes_estimate'] for note in notes if 'decoded_bytes_estimate' in note],
                         [1, 2, 3])
        self.assertEqual(captured['result']['content'][0]['mimeType'], 'image/png')
        self.assertLessEqual(len(monitor.json_text(captured).encode('utf-8')), monitor.MAX_CAPTURE_BYTES + 100)
        self.assertTrue(any('retained_chars' in note for note in notes))
        self.assertEqual(original['result']['content'][3]['text'], before)
        for source in ({'x': list(range(50000))}, {'x' * (2 * monitor.MAX_CAPTURE_BYTES): 'value'}):
            value, notes = monitor.capture_message(source)
            self.assertTrue(notes)
            self.assertLess(len(monitor.json_text(value).encode()), monitor.MAX_CAPTURE_BYTES + 100)
        deep = {}
        for _ in range(100):
            deep = {'child': deep}
        self.assertTrue(monitor.capture_message(deep)[1])

    def test_parallel_out_of_order_typed_reused_and_duplicate_ids(self):
        client = self.connect()
        observer = monitor.Observer(client)
        observer.receive(message(request(1, {'path': 'integer'})))
        observer.receive(message(request('1', {'path': 'string'})))
        observer.send(message(response('1', {'stdout': 'second'})))
        observer.send(message(response(1, {'stdout': 'first'})))
        self.drain()
        rows = list(self.model.records.values())
        self.assertEqual([row['request']['id'] for row in rows], [1, '1'])
        self.assertEqual([row['response']['result']['stdout'] for row in rows], ['first', 'second'])
        self.assertEqual(len({row['id'] for row in rows}), 2)
        observer.receive(message(request(1, {'path': 'reused'})))
        observer.send(message(response(1, {'stdout': 'third'})))
        self.drain()
        self.assertEqual(len(self.model.records), 3)
        self.assertEqual(list(self.model.records.values())[-1]['response']['result']['stdout'], 'third')
        self.model.clear()
        observer.receive(message(request(4)))
        observer.receive(message(request(4)))
        observer.send(message(response(4)))
        observer.send(message(response(4)))
        self.drain()
        self.assertTrue(all(row['status'] == 'ambiguous' and 'response' not in row
                            for row in self.model.records.values()))
        self.assertFalse(observer.pending)

    def test_independent_mcp_instances_and_pending_limit(self):
        client = self.connect()
        observers = [monitor.Observer(client), monitor.Observer(client)]
        for index, observer in enumerate(observers):
            observer.receive(message(request(1, {'path': str(index)})))
        for index in (1, 0):
            observers[index].send(message(response(1, {'value': index})))
        self.drain()
        self.assertEqual([row['response']['result']['value'] for row in self.model.records.values()], [0, 1])
        self.model.clear()
        with patch.object(monitor, 'MAX_PENDING', 2):
            for index in range(3):
                observers[0].receive(message(request(index)))
        self.drain()
        self.assertEqual(len(observers[0].pending), 2)
        self.assertEqual(next(iter(self.model.records.values()))['status'], 'incomplete')

    def test_ring_history_budget_clear_and_disconnect_do_not_replay_old_content(self):
        client = self.connect()
        epoch = client.epoch()
        for index in range(monitor.SLOTS + 5):
            client.publish(epoch, event(str(index)))
        self.model.poll()
        self.assertEqual(len(self.model.records), 4)
        self.assertEqual(self.model.dropped, 5)
        self.assertEqual(self.model.lost_events, 5)
        self.assertEqual(self.model.evicted_records, 0)
        self.drain()
        self.assertEqual(len(self.model.records), monitor.SLOTS)
        self.assertFalse(any(client.memory[monitor.HEADER_BYTES:]))
        for index in range(230):
            client.publish(epoch, event('new-' + str(index)))
            self.model.poll()
        self.assertEqual(len(self.model.records), monitor.MAX_RECORDS)
        self.assertEqual(self.model.evicted_records, monitor.SLOTS + 230 - monitor.MAX_RECORDS)
        self.assertEqual(self.model.dropped, self.model.lost_events + self.model.evicted_records)
        with patch.object(monitor, 'MAX_HISTORY_BYTES', 400):
            client.publish(epoch, event('tiny'))
            self.model.poll()
        self.assertLessEqual(self.model.used, 400)
        for index in range(10):
            client.publish(epoch, event('queued-' + str(index)))
        self.model.disconnect()
        self.model.clear()
        self.drain()
        self.assertFalse(self.model.records)
        self.assertEqual((self.model.lost_events, self.model.evicted_records, self.model.dropped), (0, 0, 0))
        self.assertFalse(any(client.memory[monitor.HEADER_BYTES:]))
        self.model.close()
        self.assertEqual(client.epoch(), 0)

    def test_stream_identity_fail_open_and_transport_error_propagation(self):
        client = self.connect()
        observer = monitor.Observer(client)
        req = message(request(arguments={'path': 'raw'}))
        result = message(response(result={'stdout': 'complete'}))
        class Input:
            async def receive(self):
                return req
            async def aclose(self):
                return None
        class Output:
            def __init__(self):
                self.sent = []
            async def send(self, value):
                self.sent.append(value)
            async def aclose(self):
                return None
        async def probe():
            source, target = Input(), Output()
            read, write = monitor.ObservedInput(source, observer), monitor.ObservedOutput(target, observer)
            self.assertIs(await read.receive(), req)
            await write.send(result)
            self.assertIs(target.sent[0], result)
            self.drain()
            self.assertEqual(next(iter(self.model.records.values()))['response'], response(result={'stdout': 'complete'}))
            for method in ('receive', 'send'):
                with patch.object(observer, method, side_effect=ValueError('monitor failed')):
                    self.assertIs(await read.receive(), req)
                    await write.send(result)
            self.model.clear()
            observer.receive(req)
            async def failed(value):
                raise BrokenPipeError('SYNTHETIC_TRANSPORT_ERROR')
            with patch.object(target, 'send', failed), self.assertRaises(BrokenPipeError):
                await write.send(result)
            self.drain()
            self.assertEqual(list(self.model.records.values())[-1]['status'], 'transport_error')
            observer.receive(req)
            cancelled = message(dict(jsonrpc='2.0', id=1, error={'code': 0, 'message': 'Request cancelled'}))
            await write.send(cancelled)
            self.drain()
            self.assertEqual(list(self.model.records.values())[-1]['status'], 'cancelled')
        asyncio.run(probe())

    def test_busy_channel_and_initialization_failure_do_not_block_tools(self):
        client = self.connect()
        acquired, release = threading.Event(), threading.Event()
        def hold():
            with self.model.channel.locked() as locked:
                self.assertTrue(locked)
                acquired.set()
                release.wait(5)
        worker = threading.Thread(target=hold)
        worker.start()
        try:
            self.assertTrue(acquired.wait(2))
            started = time.monotonic()
            client.publish(client.epoch(), event('busy'))
            self.assertLess(time.monotonic() - started, .2)
        finally:
            release.set()
            worker.join(2)
        server = FastMCP('fixture')
        original = server._mcp_server.run
        with patch.dict(os.environ, {monitor.ENV_NAME: 'invalid'}):
            monitor.install_from_environment(server)
            self.assertNotIn(monitor.ENV_NAME, os.environ)
        self.assertEqual(server._mcp_server.run, original)
        with patch.object(monitor, 'MemoryChannel', side_effect=OSError('synthetic')):
            self.model.begin_connection()
        self.assertTrue(self.model.failed)
        self.assertFalse(self.model.enabled)

    def test_owner_exit_disables_collection_without_gui_cleanup(self):
        program = '''import sys
from mcp_local.request_monitor import Monitor
m=Monitor(); m.begin_connection()
print(m.channel.token, flush=True)
sys.stdin.readline()
'''
        process = subprocess.Popen([sys.executable, '-B', '-c', program], cwd=PROJECT,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            creationflags=subprocess.CREATE_NO_WINDOW)
        client = None
        try:
            client = monitor.MemoryChannel(process.stdout.readline().decode().strip())
            self.assertGreater(client.epoch(), 0)
            process.stdin.write(b'quit\n'); process.stdin.flush()
            process.wait(timeout=10)
            self.assertEqual(client.epoch(), 0)
        finally:
            if client:
                client.close()
            if process.poll() is None:
                process.kill(); process.wait(timeout=5)
            for stream in (process.stdin, process.stdout, process.stderr):
                stream.close()

    def test_large_real_stdio_response_composes_with_existing_trace(self):
        from mcp import ClientSession, StdioServerParameters
        from mcp.client.stdio import stdio_client
        self.connect()
        program = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from mcp.server.fastmcp import FastMCP
from mcp_local import read_call_trace
from mcp_local.request_monitor import install_from_environment
server=FastMCP('synthetic-large-response')
from typing import Any
async def debug_probe() -> dict[str, Any]:
    return {'stdout':'SYNTHETIC_PRIVATE:'+'x'*(2*1024*1024)+'END_MARKER',
            'stderr':'full error details', 'unknown':{'nullable':None}}
server.add_tool(debug_probe, structured_output=True)
read_call_trace.PROJECT=Path(sys.argv[2])
read_call_trace.install(server, Path(sys.argv[2])/('.test-tmp/read-host-trace-'+'a'*32))
install_from_environment(server)
server.run(transport='stdio')
'''
        with tempfile.TemporaryDirectory(prefix='mcp-monitor-large-') as temporary:
            async def probe():
                parameters = StdioServerParameters(command=sys.executable,
                    args=['-I', '-B', '-c', program, str(PROJECT), temporary],
                    env=self.model.environment())
                async with stdio_client(parameters) as (read, write):
                    async with ClientSession(read, write) as session:
                        await session.initialize()
                        result = await session.call_tool('debug_probe', {})
                        self.assertFalse(result.isError)
                        self.assertEqual(len(result.structuredContent['stdout']),
                                         len('SYNTHETIC_PRIVATE:END_MARKER') + 2 * 1024 * 1024)
                        self.assertTrue(result.structuredContent['stdout'].endswith('END_MARKER'))
                        self.assertEqual(result.structuredContent['stderr'], 'full error details')
                        self.assertEqual(json.loads(result.content[0].text), result.structuredContent)
            asyncio.run(probe())
            self.drain()
            row = next(iter(self.model.records.values()))
            self.assertEqual(row['status'], 'success')
            self.assertTrue(row['response_notes'])
            self.assertIn('SYNTHETIC_PRIVATE', monitor.json_text(row['response']))
            self.assertLess(len(monitor.json_text(row['response']).encode()), monitor.MAX_CAPTURE_BYTES + 100)
            logs = list(Path(temporary).rglob('*.jsonl'))
            self.assertEqual(len(logs), 1)
            self.assertNotIn('SYNTHETIC_PRIVATE', logs[0].read_text())

    def test_connection_lifecycle_inheritance_failure_and_pause_reset(self):
        from mcp_local.connection_runtime import Connection
        program = '''import os,sys
sys.path.insert(0,sys.argv[1])
from mcp_local.request_monitor import MemoryChannel,ENV_NAME
c=MemoryChannel(os.environ[ENV_NAME])
assert c.epoch()
c.publish(c.epoch(), {'id':'bridge','time':'2026-11-01T00:00:00.000+08:00','tool':'fixture','status':'success'})
c.close()
'''
        with patch('mcp_local.connection_runtime.CONNECTION_MUTEX', 'Local\\MCP_Test_' + uuid4().hex), patch(
                'mcp_local.connection_runtime.existing_tunnel', return_value=False), patch(
                'mcp_local.connection_runtime.build_commands',
                return_value=[[sys.executable, '-I', '-B', '-c', program, str(PROJECT)]]), patch.dict(
                os.environ, {monitor.ENV_NAME: 'b' * 64}):
            connection = Connection(monitor=self.model)
            connection.start({'settings_version': 8}, 'SYNTHETIC_KEY')
            connection.thread.join(10)
            self.assertFalse(connection.thread.is_alive())
            self.drain()
            self.assertIn('bridge', self.model.records)
            self.assertFalse(self.model.connected)
            self.assertFalse(connection.cleanup_failed)
            previous = self.model.channel.token
            self.model.set_enabled(False)
            connection.start({'settings_version': 8}, 'SYNTHETIC_KEY')
            connection.thread.join(10)
            self.assertNotEqual(self.model.channel.token, previous)
            self.drain()
            self.assertFalse(self.model.enabled)
        with patch('mcp_local.connection_runtime.require_migration', side_effect=ValueError('fixture')):
            connection = Connection(monitor=self.model)
            connection.start({}, 'SYNTHETIC_KEY')
            connection.thread.join(5)
            self.model.poll()
            self.assertFalse(self.model.enabled)
            self.assertFalse(self.model.connected)

    def test_real_stdio_modes_full_json_and_response_bytes_equivalent(self):
        with tempfile.TemporaryDirectory(prefix='mcp-monitor-detail-') as temporary:
            root = Path(temporary)
            (root / 'sample.txt').write_text('SYNTHETIC_FILE_SECRET\nsecond\n', encoding='utf-8')
            async def probe(mode, enabled):
                self.model.begin_connection()
                self.model.clear()
                self.model.set_enabled(bool(enabled))
                env = get_default_environment()
                if enabled is not None:
                    env.update(self.model.environment())
                process = await asyncio.create_subprocess_exec(sys.executable, '-I', '-B', '-c',
                    BOOTSTRAP, str(PROJECT), str(root), mode, env=env,
                    stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE, limit=2 * 1024 * 1024,
                    creationflags=subprocess.CREATE_NO_WINDOW)
                stderr = asyncio.create_task(process.stderr.read())
                number = 0
                sent_calls = []
                async def call(method, params, notification=False):
                    nonlocal number
                    number += 1
                    value = dict(jsonrpc='2.0', method=method, params=params)
                    if not notification:
                        value['id'] = number
                    if method == 'tools/call':
                        sent_calls.append(value)
                    process.stdin.write((json.dumps(value) + '\n').encode())
                    await process.stdin.drain()
                    if notification:
                        return
                    while True:
                        raw = await asyncio.wait_for(process.stdout.readline(), 20)
                        self.assertTrue(raw)
                        if json.loads(raw).get('id') == number:
                            return raw
                try:
                    await call('initialize', dict(protocolVersion='2025-11-25', capabilities={},
                        clientInfo=dict(name='synthetic', version='1')))
                    await call('notifications/initialized', {}, True)
                    listing = await call('tools/list', {})
                    verify_contract(types.ListToolsResult.model_validate(json.loads(listing)['result']).tools, mode)
                    replies = []
                    for name, args in [('read_file', {'path': 'sample.txt', 'line_count': 2}),
                                       ('read_file', {'path': 'sample.txt', 'line_count': 'bad-SYNTHETIC'}),
                                       ('read_file', {'path': '../SYNTHETIC_PRIVATE'}),
                                       ('missing-SYNTHETIC_TOOL', {'secret': 'SYNTHETIC_TOKEN'})]:
                        replies.append(await call('tools/call', dict(name=name, arguments=args)))
                    replies.append(await call('tools/call', None))
                    self.assertIn(b'SYNTHETIC_FILE_SECRET', replies[0])
                    self.assertTrue(json.loads(replies[1])['result']['isError'])
                    self.drain()
                    if enabled:
                        rows = list(self.model.records.values())
                        self.assertEqual([row['request'] for row in rows], sent_calls)
                        self.assertEqual([row['response'] for row in rows], [json.loads(raw) for raw in replies])
                        self.assertTrue(all(not row['request_notes'] and not row['response_notes'] for row in rows))
                        self.assertEqual([row['status'] for row in rows], ['success', 'error', 'error', 'error', 'error'])
                    else:
                        self.assertFalse(self.model.records)
                    return listing, replies
                finally:
                    process.stdin.close()
                    try:
                        await asyncio.wait_for(process.wait(), 10)
                    except asyncio.TimeoutError:
                        process.kill(); await process.wait()
                    diagnostic = await stderr
                    self.assertEqual(process.returncode, 0, diagnostic.decode(errors='replace'))
            async def compare():
                for mode in ('read_only', 'developer_control', 'host_control'):
                    with self.subTest(mode=mode):
                        baseline = await probe(mode, None)
                        self.assertEqual(baseline, await probe(mode, False))
                        self.assertEqual(baseline, await probe(mode, True))
            asyncio.run(compare())

    def test_desktop_auto_pause_reconnect_complete_capture_and_clear(self):
        from mcp_local.desktop_controller import DesktopController as App
        from mcp_local.connection_settings import normalize_connection
        with tempfile.TemporaryDirectory(prefix='mcp-monitor-gui-') as temporary:
            settings = normalize_connection({'root': temporary, 'tunnel': 'tunnel_fixture',
                'roots': [{'id': 'main', 'path': temporary}], 'default_root': 'main'})
            with patch('mcp_local.tray_windows.Tray'), patch('mcp_local.desktop_controller.Connection') as factory:
                window = Scheduler(); window.withdraw()
                app = App(window, settings)
                fake = factory.return_value
                fake.cleanup_failed = False
                fake.thread = None
                fake.events = __import__('queue').Queue()
                fake.start.side_effect = lambda *args: app.request_monitor.begin_connection()
                try:
                    self.assertFalse(app.request_monitor.connected)
                    app.launch()
                    self.assertTrue(app.request_monitor.enabled)
                    app.refresh_request_monitor()
                    self.assertTrue(app.request_monitor.enabled)
                    from mcp import ClientSession, StdioServerParameters
                    from mcp.client.stdio import stdio_client
                    (Path(temporary) / 'sample.txt').write_text('SYNTHETIC_PRIVATE', encoding='utf-8')
                    async def actual_calls():
                        parameters = StdioServerParameters(command=sys.executable,
                            args=['-I', '-B', '-c', BOOTSTRAP, str(PROJECT), temporary, 'read_only'],
                            env=app.request_monitor.environment())
                        async with stdio_client(parameters) as (read, write):
                            async with ClientSession(read, write) as session:
                                await session.initialize()
                                results = await asyncio.gather(
                                    session.call_tool('read_file', {'path': 'sample.txt', 'line_count': 1}),
                                    session.call_tool('read_file', {'path': 'absent.txt'}))
                                self.assertFalse(results[0].isError)
                                self.assertTrue(results[1].isError)
                    asyncio.run(actual_calls())
                    for _ in range(4):
                        app.refresh_request_monitor()
                    from mcp_local.desktop_bridge import DesktopApi
                    api = DesktopApi(app)
                    keys = list(app.request_monitor.records)
                    self.assertEqual(len(keys), 2)
                    detail = api.dispatch('detail', {'record_id': keys[0]})['document']
                    self.assertIn('sample.txt', str(detail['record']['request']))
                    self.assertIn('SYNTHETIC_PRIVATE', str(detail['record']['response']))
                    client = monitor.MemoryChannel(app.request_monitor.channel.token)
                    try:
                        observer = monitor.Observer(client)
                        observer.receive(message(request(999)))
                        observer.send(message(response(999, {'stdout': 'LARGE_' * monitor.MAX_CAPTURE_BYTES})))
                        app.refresh_request_monitor()
                        key = list(app.request_monitor.records)[-1]
                        detail = api.dispatch('detail', {'record_id': key})['document']
                        self.assertTrue(detail['record']['response_notes'])
                        self.assertGreater(len(str(detail['record']['response'])), monitor.DETAIL_PAGE_CHARS)
                    finally:
                        client.close()
                    app.toggle_request_monitor()
                    self.assertFalse(app.request_monitor.enabled)
                    app.toggle_request_monitor()
                    self.assertTrue(app.request_monitor.enabled)
                    fake.events.put(('done', 'fixture ended'))
                    app.tick()
                    self.assertFalse(app.request_monitor.connected)
                    app.launch()
                    self.assertTrue(app.request_monitor.enabled)
                    app.toggle_request_monitor()
                    self.assertFalse(app.request_monitor.enabled)
                    app.stop()
                    fake.events.put(('done', 'stopped'))
                    app.tick()
                    app.launch()
                    self.assertTrue(app.request_monitor.enabled)
                    app.clear_request_monitor()
                    self.assertFalse(api.dispatch('monitor', {})['records'])
                    self.assertIsNone(api.dispatch('detail', {'record_id': key})['document'])
                    self.assertFalse(app.dirty)
                finally:
                    app.running = False
                    app.finish()


if __name__ == '__main__':
    unittest.main()
