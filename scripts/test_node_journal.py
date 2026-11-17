import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location('agenct_node', Path(__file__).with_name('agenct-node.py'))
node = importlib.util.module_from_spec(spec)
spec.loader.exec_module(node)


class NodeJournalTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.path = str(Path(self.tmp.name) / 'pending.json')
        self.state = {'journal_path':self.path,'connected':True,'agent_mode':'template','node_env':'test'}
        self.job = {'task_id':'task','task_revision':1,'lease_token':'a'*64,'summary':'test'}

    def test_lost_response_retries_saved_result_without_reexecuting(self):
        submitted = []
        def http(url, **kwargs):
            if url.endswith('/work'): return {'node':{'id':'pc'},'job':self.job}
            if url.endswith('/lease'): return {'lease_until':9999999999}
            submitted.append(kwargs['body'].copy())
            if len(submitted) == 1: raise OSError('response lost after commit')
            return {'status':'completed'}
        with patch.object(node,'http_json',side_effect=http), patch.object(node,'run_job',return_value=('completed','result','template',[])) as run:
            with self.assertRaises(OSError): node.poll_once('http://hub','token',300,self.state)
            saved = json.loads(Path(self.path).read_text())
            self.assertEqual(saved['phase'],'result')
            # New process state; disk receipt, not memory, drives recovery.
            self.assertTrue(node.poll_once('http://hub','token',300,dict(self.state)))
            self.assertEqual(run.call_count,1)
        self.assertEqual(submitted[0],submitted[1])
        self.assertFalse(Path(self.path).exists())

    def test_crashed_execution_is_archived_and_never_restarted(self):
        node.save_pending(self.path,{'phase':'executing','task_id':'task','body':{'lease_token':'a'*64}})
        with patch.object(node,'http_json') as http, patch.object(node,'run_job') as run:
            self.assertFalse(node.poll_once('http://hub','token',300,self.state))
            http.assert_not_called(); run.assert_not_called()
        self.assertEqual(len(list(Path(self.tmp.name).glob('uncertain-*.json'))),1)

    def test_rejected_expired_result_is_retained_without_repeat_execution(self):
        node.save_pending(self.path,{'phase':'result','task_id':'task','body':{'request_id':'same','content':'result'}})
        error = urllib.error.HTTPError('http://hub',409,'expired',{},None)
        with patch.object(node,'http_json',side_effect=error), patch.object(node,'run_job') as run:
            self.assertFalse(node.poll_once('http://hub','token',300,self.state))
            run.assert_not_called()
        self.assertEqual(len(list(Path(self.tmp.name).glob('rejected-result-*.json'))),1)

    def test_journal_is_written_before_execution_and_no_lease_never_executes(self):
        def run(*args, **kwargs):
            self.assertEqual(json.loads(Path(self.path).read_text())['phase'],'executing')
            self.assertEqual(Path(self.path).stat().st_mode & 0o777,0o600)
            return 'completed','result','template',[]
        def http(url,**kwargs): return {'node':{},'job':self.job} if url.endswith('/work') else {}
        with patch.object(node,'http_json',side_effect=http), patch.object(node,'run_job',side_effect=run):
            node.poll_once('http://hub','token',300,self.state)
        self.job.pop('lease_token')
        with patch.object(node,'http_json',side_effect=http), patch.object(node,'run_job') as run:
            with self.assertRaises(RuntimeError): node.poll_once('http://hub','token',300,self.state)
            run.assert_not_called()

    def test_same_local_state_is_exclusively_locked(self):
        guard=node.lock_node_state(self.tmp.name)
        try:
            with self.assertRaises(RuntimeError): node.lock_node_state(self.tmp.name)
        finally: guard.close()


if __name__ == '__main__':
    unittest.main()
