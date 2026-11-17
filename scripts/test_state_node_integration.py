"""Real Hub/SQLite/HTTP recovery tests. No provider calls or local CLI execution."""
import base64
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import urllib.error
import state_backup

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get('AGENCT_TEST_HUB_BINARY', ROOT / 'hub/target/debug/agenct-hub'))
spec = importlib.util.spec_from_file_location('node_recovery', ROOT / 'scripts/agenct-node.py')
node = importlib.util.module_from_spec(spec)
spec.loader.exec_module(node)


@unittest.skipUnless(BINARY.is_file(), 'build the Hub before real-process recovery tests')
class StateNodeIntegrationTests(unittest.TestCase):
    def test_lost_submission_restart_and_complete_state_restore(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / 'state'; state.mkdir(mode=0o700)
            tokens = {u:secrets.token_hex(32) for u in ('alice','bob')}
            users = [{'id':u,'token_sha256':hashlib.sha256(t.encode()).hexdigest()} for u,t in tokens.items()]
            (state/'config.json').write_text(json.dumps({'version':1,'runtime':'mock','port':8080,'users':users}))
            (state/'credentials.json').write_text(json.dumps(tokens))
            journal = root/'node'; journal.mkdir(mode=0o700)
            process = None
            def start(directory):
                nonlocal process
                with socket.socket() as sock:
                    sock.bind(('127.0.0.1',0)); port=sock.getsockname()[1]
                origin=f'http://127.0.0.1:{port}'
                process=subprocess.Popen([str(BINARY)], cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                    env={'PATH':os.environ.get('PATH',''), 'AGENCT_RUNTIME':'mock','AGENCT_AUTO_DRAFT':'0','AGENCT_BIND':f'127.0.0.1:{port}',
                         'AGENCT_USERS_JSON':json.dumps(users),'AGENCT_DATABASE':str(directory/'hub.sqlite3')})
                for _ in range(100):
                    try:
                        node.http_json(origin+'/health'); return origin
                    except (OSError,urllib.error.URLError): time.sleep(.05)
                raise AssertionError('Hub did not start')
            def stop():
                nonlocal process
                if process and process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try: process.wait(timeout=5)
                    except subprocess.TimeoutExpired: process.kill(); process.wait()
                process=None
            try:
                origin=start(state)
                def api(who,path,method='GET',body=None):
                    return node.http_json(origin+'/api/'+path, token=tokens[who],method=method,body=body)
                created=api('bob','nodes','POST',{'id':'pc','name':'PC'})
                node_token=created['token']
                p=api('alice','proposals','POST',{'owner':'bob','summary':'Write a report','request_id':'task'})
                task=api('alice','proposals/'+p['id']+'/actions','POST',{'action':'confirm','expected_revision':0})['task_id']
                api('bob',f'tasks/{task}/actions','POST',{'action':'accept','expected_revision':0})
                api('bob',f'tasks/{task}/node-assignment','POST',{'node_id':'pc','expected_revision':1})
                api('bob','automation/rules','POST',{'keyword':'reports','target_node_id':'pc'})
                png=base64.b64encode(bytes.fromhex('89504e470d0a1a0a')+b'fixture').decode()
                attachment=api('bob','attachments','POST',{'name':'proof.png','mime_type':'image/png','data_base64':png})
                with self.assertRaisesRegex(ValueError, 'stop the Hub'):
                    state_backup.create(state,root/'must-not-exist.zip')
                self.assertFalse((root/'must-not-exist.zip').exists())
                real_http=node.http_json
                lost = False
                def transport(url,**kwargs):
                    nonlocal lost
                    result=real_http(url,**kwargs)
                    if url.endswith('/drafts') and kwargs.get('method')=='POST' and not lost:
                        lost=True
                        raise OSError('simulated response lost AFTER server commit')
                    return result
                client={'journal_path':str(journal/'pending.json'),'connected':True,'node_env':'integration','agent_mode':'template'}
                with patch.object(node,'http_json',side_effect=transport),patch.object(node,'run_job',return_value=('completed','retained output','template',[])) as run:
                    with self.assertRaises(OSError): node.poll_once(origin,node_token,300,client)
                    self.assertEqual(run.call_count,1)
                self.assertEqual(len(api('bob',f'tasks/{task}/drafts')['items']),1)
                stop()
                origin=start(state)
                with patch.object(node,'run_job') as run:
                    self.assertTrue(node.poll_once(origin,node_token,300,client))
                    run.assert_not_called()
                self.assertEqual(len(api('bob',f'tasks/{task}/drafts')['items']),1)
                stop()
                archive=root/'full.zip'
                subprocess.run([sys.executable,str(ROOT/"scripts/private-staging.py"),"backup-state","--state-dir",str(state),"--backup-file",str(archive)],check=True,stdout=subprocess.DEVNULL)
                restored=root/'restored'
                subprocess.run([sys.executable,str(ROOT/"scripts/private-staging.py"),"restore-state","--state-dir",str(restored),"--backup-file",str(archive)],check=True,stdout=subprocess.DEVNULL)
                origin=start(restored)
                self.assertEqual(len(api('bob',f'tasks/{task}/drafts')['items']),1)
                self.assertEqual(len(api('bob','automation')['rules']),1)
                self.assertEqual(api('bob','attachments/'+attachment['id'])['data_base64'],png)
                self.assertEqual(json.loads((restored/'credentials.json').read_text()),tokens)
            finally: stop()


if __name__ == '__main__': unittest.main()
