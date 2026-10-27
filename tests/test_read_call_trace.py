"""診斷不變更回應／契約、不保存內容，並能關聯單次請求與 STDIO flush。"""
import asyncio
import hashlib
import json
from pathlib import Path
import tempfile
import sys
import unittest
from unittest.mock import patch

from mcp import types
from mcp.client.stdio import get_default_environment
from mcp_local import read_call_trace as trace_module
from mcp_local.tool_contract import verify_contract


PROJECT = Path(__file__).resolve().parents[1]
BOOTSTRAP = '''import sys
from pathlib import Path
source,fixture,trace=sys.argv[1:4]
sys.path.insert(0,source)
from mcp_local import security_policy
security_policy.STATE_DIR=Path(fixture).parent/'synthetic-state'
from mcp_local import read_call_trace
read_call_trace.PROJECT=Path(fixture)/'MCP-Local'
if sys.argv[4]=='io_failed':
    original=read_call_trace.Trace.record
    def fail_record(self,stage,**fields):
        if stage=='request_received':self.handle.close()
        return original(self,stage,**fields)
    read_call_trace.Trace.record=fail_record
from mcp_local import local_files_mcp
local_files_mcp.__file__=str(read_call_trace.PROJECT/'local_files_mcp.py')
if trace!='off':
    request=read_call_trace.PROJECT/'.test-tmp/read-host-trace.request'
    request.parent.mkdir(exist_ok=True)
    request.write_text(Path(trace).name,encoding='ascii')
sys.argv=['local_files_mcp.py','--root',fixture,'--access-mode','host_control']
local_files_mcp.main()
'''


class ReadCallTraceTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix='mcp-read-trace-')
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name).resolve()
        self.project = self.base / 'MCP-Local'
        self.project.mkdir()
        self.folder = self.project / ('.test-tmp/read-host-trace-' + 'a' * 32)
        self.project_patch = patch.object(trace_module, 'PROJECT', self.project)
        self.project_patch.start()
        self.addCleanup(self.project_patch.stop)

    def create_trace(self):
        trace = trace_module.Trace(self.folder)
        self.addCleanup(trace.close)
        return trace

    def test_request_privacy_shape_and_limits(self):
        trace = self.create_trace()
        path = str(self.project / 'shared/README.md')
        def receive(identifier):
            message = types.JSONRPCMessage(types.JSONRPCRequest(jsonrpc='2.0', id=identifier,
                method='tools/call', params=dict(name='read_host_file', arguments=dict(path=path, line_count=2))))
            trace.receive(type('Message', (), dict(message=message))())
        receive('SYNTHETIC_SECRET_REQUEST_ID')
        trace.shape_event('SYNTHETIC_SECRET_REQUEST_ID', 'mcp_result', dict(content=[dict(type='text',
            text=json.dumps(dict(content='SYNTHETIC_SECRET_FILE', path='SYNTHETIC_PRIVATE_PATH')))],
            structuredContent=dict(token='SYNTHETIC_CREDENTIAL'), isError=False))
        text = Path(trace.handle.name).read_text(encoding='utf-8')
        for value in ('SYNTHETIC_SECRET_REQUEST_ID', 'SYNTHETIC_SECRET_FILE',
                      'SYNTHETIC_PRIVATE_PATH', 'SYNTHETIC_CREDENTIAL', path):
            self.assertNotIn(value, text)
        row = json.loads(text.splitlines()[-1])
        self.assertTrue(row['has_content'])
        self.assertFalse(row['isError'])
        self.assertEqual(row['structured_type'], 'object')
        with patch.object(trace_module, 'MAX_CALLS', 1):
            receive('second')
        self.assertTrue(trace.stopped)
        self.assertEqual(len(trace.calls), 1)

    def test_log_failure_expiry_and_output_cap_do_not_raise(self):
        trace = self.create_trace()
        with patch.object(trace_module, 'MAX_BYTES', trace.used):
            trace.record('bounded')
        self.assertTrue(trace.stopped)
        trace.stopped = False
        trace.handle.close()
        trace.record('disk_failed')
        self.assertTrue(trace.stopped)
        trace.close()

    def test_sequential_reused_id_concurrent_duplicate_and_expiry(self):
        trace = self.create_trace()
        path = str(self.project / 'shared/README.md')
        message = types.JSONRPCMessage(types.JSONRPCRequest(jsonrpc='2.0', id='reused',
            method='tools/call', params=dict(name='read_host_file', arguments=dict(path=path, line_count=2))))
        wrapped = type('Message', (), dict(message=message))()
        trace.receive(wrapped)
        key = trace.request_key('reused')
        trace.completed.add(key)
        trace.receive(wrapped)
        self.assertEqual(trace.accepted, 2)
        self.assertEqual(trace.calls[key]['call'], 2)
        trace.receive(wrapped)
        self.assertIn(key, trace.ambiguous)
        length = trace.used
        trace.event('reused', 'must_not_attribute')
        self.assertEqual(trace.used, length)
        trace.started -= trace_module.MAX_SECONDS + 1
        trace.receive(wrapped)
        self.assertTrue(trace.stopped)

    def test_directory_escape_and_links_are_rejected(self):
        with self.assertRaises(ValueError):
            trace_module.Trace(self.base / 'outside')
        temporary = self.project / '.test-tmp'
        temporary.symlink_to(self.base / 'elsewhere', target_is_directory=True)
        with self.assertRaises((ValueError, FileNotFoundError)):
            trace_module.Trace(self.folder)

    def test_one_shot_request_rejects_untrusted_paths_and_is_consumed(self):
        request = self.project / '.test-tmp/read-host-trace.request'
        request.parent.mkdir()
        for value in ('../../outside', 'read-host-trace-' + 'z' * 32, 'x' * 81):
            request.write_text(value, encoding='ascii')
            with self.assertRaises(ValueError):
                trace_module.consume_request(request)
            self.assertTrue(request.exists())
        request.write_text(self.folder.name, encoding='ascii')
        self.assertEqual(trace_module.consume_request(request), self.folder)
        self.assertFalse(request.exists())

    def test_writer_preserves_bytes_and_propagates_transport_failure(self):
        trace = self.create_trace()
        class Output:
            def __init__(self):
                self.values = []
            async def write(self, data):
                self.values.append(data)
                return len(data)
            async def flush(self):
                return None
        async def run():
            stream = Output()
            writer = trace_module.ObservedOutput(stream, trace)
            value = '{"jsonrpc":"2.0","id":"request","result":{"content":[],"isError":false}}\n'
            self.assertEqual(await writer.write(value), len(value))
            await writer.flush()
            self.assertEqual(stream.values, [value])
            async def failed(*args):
                raise BrokenPipeError('SYNTHETIC_TRANSPORT_SECRET')
            with patch.object(stream, 'flush', failed), self.assertRaises(BrokenPipeError):
                await writer.flush()
            with patch.object(stream, 'write', failed), self.assertRaises(BrokenPipeError):
                await writer.write(value)
        asyncio.run(run())

    def test_real_stdio_equivalence_correlation_and_redaction(self):
        shared = self.project / 'shared'
        shared.mkdir()
        sample = shared / 'README.md'
        sample.write_text('SYNTHETIC_SECRET_FILE\nsecond\nthird\n', encoding='utf-8')
        before = hashlib.sha256(sample.read_bytes()).hexdigest()

        async def probe(enabled, examples=None, failure=False):
            folder = self.folder if not failure else self.project / ('.test-tmp/read-host-trace-' + 'b' * 32)
            process = await asyncio.create_subprocess_exec(sys.executable, '-I', '-B', '-c', BOOTSTRAP,
                str(PROJECT), str(self.base), str(folder) if enabled else 'off', 'io_failed' if failure else 'normal',
                env=get_default_environment(), stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, limit=2 * 1024 * 1024)
            stderr = asyncio.create_task(process.stderr.read())
            number = 0
            async def request(method, params, notification=False):
                nonlocal number
                number += 1
                message = dict(jsonrpc='2.0', method=method, params=params)
                if not notification:
                    message['id'] = 'SYNTHETIC_SECRET_ID_' + str(number)
                process.stdin.write((json.dumps(message) + '\n').encode())
                await process.stdin.drain()
                if notification:
                    return
                while True:
                    raw = await asyncio.wait_for(process.stdout.readline(), 20)
                    self.assertTrue(raw)
                    value = json.loads(raw)
                    if value.get('id') == message['id']:
                        self.assertNotIn('error', value)
                        return raw
            try:
                await request('initialize', dict(protocolVersion='2025-11-25', capabilities={},
                    clientInfo=dict(name='SYNTHETIC_CLIENT_SECRET', version='1')))
                await request('notifications/initialized', {}, True)
                listing = await request('tools/list', {})
                verify_contract(types.ListToolsResult.model_validate(json.loads(listing)['result']).tools, 'host_control')
                if enabled and examples is None:
                    self.assertFalse((self.project / '.test-tmp/read-host-trace.request').exists())
                    plans = list(self.folder.glob('*.plan.json'))
                    self.assertEqual(len(plans), 1)
                    examples = json.loads(plans[0].read_text())['examples']
                results = []
                ordinary = [dict(tool='read_host_file', arguments=dict(path=str(sample), line_count=2)),
                            dict(tool='read_host_file', arguments=dict(path='MCP-Local/shared/README.md', line_count=2)),
                            dict(tool='read_file', arguments=dict(path='MCP-Local/shared/README.md', line_count=2))]
                for example in ordinary + examples:
                    raw = await request('tools/call', dict(name=example['tool'], arguments=example['arguments']))
                    results.append(raw)
                    value = json.loads(json.loads(raw)['result']['content'][0]['text'])
                    self.assertEqual(value.get('text', value.get('content')), '1: SYNTHETIC_SECRET_FILE\n2: second')
                # Existing StrictArguments and protection errors retain their response.
                for arguments in (dict(path=str(sample), line_count='2'),
                                  dict(path=str(sample), line_count=2, root_id='SYNTHETIC_PRIVATE_ROOT')):
                    raw = await request('tools/call', dict(name='read_host_file', arguments=arguments))
                    self.assertTrue(json.loads(raw)['result']['isError'])
                    results.append(raw)
                # Diagnostic output remains excluded by both existing file APIs.
                for name, path in (('read_host_file', str(self.folder / 'log.jsonl')),
                                   ('read_file', 'MCP-Local/.test-tmp/log.jsonl')):
                    raw = await request('tools/call', dict(name=name, arguments=dict(path=path)))
                    self.assertTrue(json.loads(raw)['result']['isError'])
                    results.append(raw)
                return listing, results, examples
            finally:
                process.stdin.close()
                try:
                    await asyncio.wait_for(process.wait(), 10)
                except asyncio.TimeoutError:
                    process.terminate()
                    await process.wait()
                await stderr
                self.assertEqual(process.returncode, 0)

        async def compare():
            traced = await probe(True)
            baseline = await probe(False, traced[2])
            self.assertEqual(traced[:2], baseline[:2], 'All tools/list and tools/call bytes must match')
            failed_log = await probe(True, traced[2], failure=True)
            self.assertEqual(failed_log[:2], baseline[:2], 'Log I/O failure must not change response bytes')
        asyncio.run(compare())
        self.assertEqual(hashlib.sha256(sample.read_bytes()).hexdigest(), before)
        logs = list(self.folder.glob('*.jsonl'))
        self.assertEqual(len(logs), 1)
        text = logs[0].read_text()
        self.assertNotIn('SYNTHETIC_', text)
        self.assertNotIn(str(self.base), text)
        rows = [json.loads(line) for line in text.splitlines()]
        received = [row for row in rows if row['stage'] == 'request_received']
        self.assertEqual(len(received), 10)
        self.assertEqual(len({row['request'] for row in received}), 10)
        for row in received:
            events = [r['stage'] for r in rows if r.get('request') == row['request']]
            self.assertEqual(events[0], 'request_received')
            self.assertIn('mcp_result', events)
            self.assertIn('stdout_written', events)
            self.assertIn('stdout_flushed', events)
            if row['call'] <= 9:
                self.assertIn('tool_result', events)
        self.assertEqual(rows[-1]['stage'], 'trace_closed')


if __name__ == '__main__':
    unittest.main()
