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

    def test_materialize_attachments_preserves_pdf_txt_and_separates_multimodal_images(self):
        import base64
        import shutil
        job = {
            'summary': '解析附件',
            'attachments': [
                {'name': 'spec.pdf', 'mime_type': 'application/pdf', 'data_base64': base64.b64encode(b'%PDF-1.4\n').decode()},
                {'name': 'notes.txt', 'mime_type': 'text/plain', 'data_base64': base64.b64encode(b'hello txt').decode()},
                {'name': 'bundle.zip', 'mime_type': 'application/zip', 'data_base64': base64.b64encode(b'PK\x05\x06' + b'\x00'*18).decode()},
                {'name': 'ui.png', 'mime_type': 'image/png', 'data_base64': base64.b64encode(b'\x89PNG\r\n\x1a\n').decode()},
            ],
        }
        paths, temp_dir = node.materialize_attachments(job)
        try:
            self.assertEqual(len(paths), 4)
            self.assertTrue(paths[0].endswith('_spec.pdf'))
            self.assertTrue(paths[1].endswith('_notes.txt'))
            self.assertTrue(paths[2].endswith('_bundle.zip'))
            self.assertTrue(paths[3].endswith('_ui.png'))
            prompt = node.build_prompt(job, image_paths=paths)
            self.assertIn('用户在本轮对话中附带了以下图片附件', prompt)
            self.assertIn('用户在本轮对话中附带了以下文件附件', prompt)
            for p in paths:
                self.assertIn(p, prompt)
        finally:
            if temp_dir:
                shutil.rmtree(temp_dir, ignore_errors=True)


if __name__ == '__main__':
    unittest.main()
