"""Exercise the Docker backup wrapper with a fake docker command; never touch Docker."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class BackupWrapperTests(unittest.TestCase):
    def run_backup(self, running=True, fail=False, full=True):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)
            shutil.copyfile(Path(__file__).resolve().parents[1]/'deploy/docker/backup.sh',root/'backup.sh')
            docker=root/'docker'
            docker.write_text('''#!/usr/bin/env python3
import json, os, pathlib, sys
args=sys.argv[1:]
with open(os.environ['FAKE_DOCKER_LOG'],'a') as stream: stream.write(json.dumps(args)+'\\n')
if 'ps' in args and os.environ['FAKE_RUNNING']=='1': print('container-id')
if 'backup-state' in args or 'exec' in args:
    pathlib.Path('data/backups').mkdir(parents=True,exist_ok=True)
    if os.environ['FAKE_FAIL']=='1': sys.exit(9)
''')
            docker.chmod(0o700)
            log=root/'calls'
            env={**os.environ,'PATH':str(root)+os.pathsep+os.environ['PATH'],'FAKE_DOCKER_LOG':str(log),'FAKE_RUNNING':str(int(running)),'FAKE_FAIL':str(int(fail))}
            result=subprocess.run(['bash',str(root/'backup.sh'),*(['--full'] if full else [])],env=env,capture_output=True)
            calls=[json.loads(line) for line in log.read_text().splitlines()]
            return result.returncode,calls

    def test_full_backup_restarts_previously_running_service_even_on_failure(self):
        for fail in (False,True):
            code,calls=self.run_backup(fail=fail)
            self.assertEqual(code,9 if fail else 0)
            self.assertIn(['compose','stop','agenct'],calls)
            self.assertEqual(calls[-1],['compose','start','agenct'])
            self.assertTrue(any('backup-state' in call for call in calls))

    def test_stopped_service_is_not_started(self):
        code,calls=self.run_backup(running=False)
        self.assertEqual(code,0)
        self.assertFalse(any('start' in call or 'stop' in call for call in calls))

    def test_default_snapshot_does_not_stop_service(self):
        code,calls=self.run_backup(full=False)
        self.assertEqual(code,0)
        self.assertTrue(any('exec' in call for call in calls))
        self.assertFalse(any('stop' in call or 'start' in call for call in calls))


if __name__=='__main__': unittest.main()
