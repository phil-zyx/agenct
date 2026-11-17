import fcntl
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
import zipfile
import state_backup as backup


class StateBackupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.state = self.root / 'state'
        self.state.mkdir(mode=0o700)
        self.archive = self.root / 'backup.zip'
        db = sqlite3.connect(self.state / 'hub.sqlite3')
        self.addCleanup(db.close)
        db.execute('PRAGMA journal_mode=WAL')
        for table in ('sessions', 'messages', 'tasks', 'proposals'):
            db.execute(f'CREATE TABLE {table}(value TEXT)')
        db.execute("INSERT INTO messages VALUES ('committed WAL content')")
        db.commit()
        (self.state / 'automation_state.json').write_text('{"rules":[{"id":"saved-rule"}]}')
        (self.state / 'credentials.json').write_text('{"alice":"private-token"}')
        (self.state / 'config.json').write_text('{"version":1}')
        (self.state / 'launch.json').write_text(json.dumps({'AGENCT_DATABASE':str(self.state / 'hub.sqlite3'), 'AGENCT_BROWSER_OIDC_CONFIG_FILE':str(self.state / 'browser-oidc.json')}))
        (self.state / 'browser-oidc.json').write_text('{"issuer":"https://idp.example"}')
        (self.state / 'attachments').mkdir()
        (self.state / 'attachments/att_one.json').write_text('{"data_base64":"cGljdHVyZQ=="}')

    def test_complete_roundtrip_wal_attachments_credentials_and_path_relocation(self):
        backup.create(self.state, self.archive)
        self.assertEqual(self.archive.stat().st_mode & 0o777, 0o600)
        restored = self.root / 'restored'
        backup.restore(self.archive, restored)
        with sqlite3.connect(restored / 'hub.sqlite3') as db:
            self.assertEqual(db.execute('SELECT value FROM messages').fetchone()[0], 'committed WAL content')
        for name in ('config.json', 'credentials.json', 'attachments/att_one.json'):
            self.assertEqual((restored / name).read_bytes(), (self.state / name).read_bytes())
            self.assertEqual((restored / name).stat().st_mode & 0o777, 0o600)
        self.assertEqual(json.loads((restored / 'automation_state.json').read_text()), json.loads((self.state / 'automation_state.json').read_text()))
        launch = json.loads((restored / 'launch.json').read_text())
        self.assertEqual(launch['AGENCT_DATABASE'], str(restored / 'hub.sqlite3'))
        self.assertFalse((restored / '.restore-in-progress').exists())
        with self.assertRaises(ValueError):
            backup.restore(self.archive, restored)
        with self.assertRaises(FileExistsError):
            backup.create(self.state, self.archive)

    def test_restore_pauses_old_execution_and_external_polling(self):
        with sqlite3.connect(self.state / 'hub.sqlite3') as db:
            db.execute('CREATE TABLE node_assignments(status TEXT)')
            db.executemany('INSERT INTO node_assignments VALUES (?)', [('queued',),('running',),('completed',)])
        (self.state / 'automation_state.json').write_text(json.dumps({'crons':[{'enabled':True,'instruction':'keep definition'}], 'gitlab_configs':{'bob':{'auto_poll_todos':True,'token':'keep credential'}}}))
        backup.create(self.state, self.archive)
        restored = self.root / 'restored'
        backup.restore(self.archive, restored)
        with sqlite3.connect(restored / 'hub.sqlite3') as db:
            self.assertEqual(db.execute('SELECT status FROM node_assignments').fetchall(), [('failed',),('failed',),('completed',)])
        data = json.loads((restored / 'automation_state.json').read_text())
        self.assertFalse(data['crons'][0]['enabled'])
        self.assertFalse(data['gitlab_configs']['bob']['auto_poll_todos'])
        self.assertEqual(data['gitlab_configs']['bob']['token'], 'keep credential')

    def test_container_restore_can_keep_the_final_mount_path(self):
        backup.create(self.state, self.archive)
        destination = self.root / 'new-volume'
        backup.restore(self.archive, destination, Path('/data'))
        self.assertEqual(json.loads((destination/'launch.json').read_text())['AGENCT_DATABASE'], '/data/hub.sqlite3')

    def test_launch_referenced_local_configs_are_included_and_external_paths_are_listed(self):
        (self.state/'custom').mkdir()
        (self.state/'custom/workers.json').write_text('[]')
        launch=json.loads((self.state/'launch.json').read_text())
        launch['AGENCT_WORKER_SCOPES_FILE']=str(self.state/'custom/workers.json')
        launch['AGENCT_REPOSITORIES_FILE']='/external/repositories.json'
        (self.state/'launch.json').write_text(json.dumps(launch))
        backup.create(self.state,self.archive)
        with zipfile.ZipFile(self.archive) as archive:
            manifest=json.loads(archive.read('manifest.json'))
            self.assertIn('custom/workers.json',manifest['files'])
            self.assertEqual(manifest['external_files'],['/external/repositories.json'])
        restored=self.root/'restored'
        backup.restore(self.archive,restored)
        self.assertEqual((restored/'custom/workers.json').read_text(),'[]')

    def test_live_hub_lock_prevents_mixed_snapshot(self):
        with (self.state / 'hub.lock').open('w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaisesRegex(ValueError, 'stop the Hub'):
                backup.create(self.state, self.archive)
        self.assertFalse(self.archive.exists())

    def test_symlink_attachment_is_rejected(self):
        (self.state / 'attachments/att_link.json').symlink_to(self.state / 'credentials.json')
        with self.assertRaises(ValueError):
            backup.create(self.state, self.archive)
        self.assertFalse(self.archive.exists())

    def test_tampering_missing_files_and_traversal_never_publish_destination(self):
        backup.create(self.state, self.archive)
        with zipfile.ZipFile(self.archive) as source:
            original = {name: source.read(name) for name in source.namelist()}
        for mode in ('tamper', 'missing', 'traversal'):
            entries = dict(original)
            if mode == 'tamper': entries['credentials.json'] = b'{}'
            elif mode == 'missing': del entries['attachments/att_one.json']
            else: entries['../outside'] = b'invalid'
            path = self.root / (mode + '.zip')
            with zipfile.ZipFile(path, 'w') as dest:
                for name, value in entries.items(): dest.writestr(name, value)
            output = self.root / mode
            with self.assertRaises(ValueError): backup.restore(path, output)
            self.assertFalse(output.exists())
        self.assertFalse((self.root / 'outside').exists())

    def test_interrupted_restore_does_not_leave_a_ready_state(self):
        from unittest.mock import patch
        backup.create(self.state, self.archive)
        output = self.root / 'interrupted'
        with patch.object(backup.shutil, 'move', side_effect=OSError('disk full')):
            with self.assertRaises(OSError): backup.restore(self.archive, output)
        self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
