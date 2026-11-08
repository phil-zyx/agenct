"""Exercise HTTP -> real worker subprocess -> fake installed SDK, without a provider."""
from contextlib import redirect_stderr
import http.client
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from adapter import DshRunner, make_server
from test_adapter import turn
from test_profile_updates import profile_context


FAKE_SDK = '''
import json, sys
from types import SimpleNamespace
class DeepSeekHarness:
    def __init__(self, **kwargs):
        assert kwargs['profile'] == 'sdk-minimal'
        assert kwargs['max_tokens'] == 4096
    def __enter__(self): return self
    def __exit__(self, *args): pass
    def run(self, prompt, session_id):
        data = json.loads(prompt.split('\\n', 1)[1])
        assert data['profile_context']['profile']['permissions']['auto_reply_public'] is False
        assert data['profile_context']['profile']['private']['work_context'] == 'OWNER_PRIVATE_CONTEXT'
        assert data['history'] == []
        # These must never escape into the HTTP result or adapter diagnostics.
        print('SENSITIVE_TEST_ONLY noisy SDK stdout')
        print('SENSITIVE_TEST_ONLY noisy SDK stderr', file=sys.stderr)
        message = data['message']
        if message == 'sdk-error': raise RuntimeError('SENSITIVE_TEST_ONLY error with key and body')
        if message == 'sdk-timeout': raise TimeoutError('SENSITIVE_TEST_ONLY timed out')
        reply = {'reply': 'offline reply', 'proposal': None}
        if message == 'profile-change': reply['profile_update'] = {'private': {'response_preferences': '先写结论'}}
        if message == 'backend-field': reply['backend'] = 'dsh'
        text = 'SENSITIVE_TEST_ONLY invalid JSON' if message == 'broken-json' else json.dumps(reply)
        return SimpleNamespace(finish_reason='failed' if message == 'incomplete' else 'completed', final_response=text)
'''


class WorkerProcessTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix='agenct-offline-sdk-')
        cls.root = Path(cls.temp.name)
        subprocess.run([sys.executable, '-m', 'venv', '--without-pip', str(cls.root / 'venv')], check=True, capture_output=True)
        cls.python = cls.root / 'venv' / 'bin' / 'python'
        site = Path(subprocess.check_output([str(cls.python), '-c', 'import sysconfig; print(sysconfig.get_paths()["purelib"])'], text=True).strip())
        (site / 'deepseek_harness').mkdir()
        (site / 'deepseek_harness' / '__init__.py').write_text(FAKE_SDK)
        for name in ('deepseek_harness_sdk', 'deepseek_harness_runtime_bin'):
            metadata = site / (name + '-0.2.1rc2.dist-info')
            metadata.mkdir()
            (metadata / 'METADATA').write_text('Metadata-Version: 2.1\nName: ' + name.replace('_', '-') + '\nVersion: 0.2.1rc2\n')

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def setUp(self):
        self.patches = [patch('adapter.sys.executable', str(self.python)),
                        patch.dict(os.environ, {'DEEPSEEK_API_KEY': 'offline-test-key-no-provider'})]
        for item in self.patches: item.start()
        self.addCleanup(lambda: [item.stop() for item in reversed(self.patches)])
        self.token = 'offline-adapter-token-' + 'x' * 40
        self.server = make_server(self.token, DshRunner(timeout=5), port=0)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.close_server)

    def close_server(self):
        self.server.shutdown(); self.server.server_close(); self.thread.join(2)

    def request(self, message):
        client = http.client.HTTPConnection(*self.server.server_address, timeout=10)
        try:
            client.request('POST', '/v1/turn', json.dumps(turn() | {'message': message, 'profile_context': profile_context()}),
                           {'Authorization': 'Bearer ' + self.token, 'Content-Type': 'application/json'})
            response = client.getresponse()
            return response.status, json.loads(response.read())
        finally:
            client.close()

    def test_profile_context_survives_real_worker_and_backend_is_added_exactly_once(self):
        status, response = self.request('profile-change')
        self.assertEqual(status, 200)
        self.assertEqual(response, {'reply': 'offline reply', 'proposal': None, 'backend': 'dsh',
                                    'profile_update': {'private': {'response_preferences': '先写结论'}}})

    def test_worker_failures_are_classified_without_echoing_private_output(self):
        logs = io.StringIO()
        with redirect_stderr(logs):
            for message, code, status in [('broken-json', 'model_protocol_error', 502),
                                          ('backend-field', 'model_protocol_error', 502),
                                          ('incomplete', 'runtime_incomplete', 502),
                                          ('sdk-error', 'runtime_failed', 502),
                                          ('sdk-timeout', 'runtime_timeout', 504)]:
                with self.subTest(message=message):
                    self.assertEqual(self.request(message), (status, {'error': code}))
        output = logs.getvalue()
        self.assertNotIn('SENSITIVE_TEST_ONLY', output)
        self.assertNotIn('OWNER_PRIVATE_CONTEXT', output)
        self.assertNotIn('offline-test-key', output)
        entries = [json.loads(line) for line in output.splitlines()]
        self.assertEqual(len(entries), 5)
        self.assertEqual(entries[0]['exception_type'], 'JSONDecodeError')
        self.assertEqual(entries[1]['exception_type'], 'ValueError')
        self.assertTrue(all(set(entry) == {'event', 'code', 'stage', 'exception_type'} for entry in entries))


if __name__ == '__main__':
    unittest.main()
