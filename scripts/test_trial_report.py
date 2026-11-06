import importlib.util
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('trial_report', Path(__file__).with_name('trial-report.py'))
trial = importlib.util.module_from_spec(spec)
spec.loader.exec_module(trial)


class TrialReportTests(unittest.TestCase):
    def test_cohorts_duration_and_private_data_excluded(self):
        with tempfile.TemporaryDirectory() as folder:
            database = Path(folder)/'hub.sqlite3'
            with sqlite3.connect(database) as db:
                db.executescript('''
                    CREATE TABLE proposals(status TEXT, created_at TEXT);
                    CREATE TABLE deliveries(id TEXT,task_id TEXT,status TEXT,created_at TEXT,source_draft_id TEXT,content TEXT);
                    CREATE TABLE events(task_id TEXT,action TEXT,at TEXT);
                    CREATE TABLE tasks(status TEXT);
                    CREATE TABLE messages(status TEXT,created_at TEXT);
                    CREATE TABLE chat_turns(status TEXT);
                    INSERT INTO proposals VALUES ('confirmed','2026-11-06'),('rejected','2026-11-06'),('pending','2026-11-06');
                    INSERT INTO deliveries VALUES ('d','t','draft','2026-11-06',NULL,'TOP SECRET'),('s','t','submitted','2026-11-06',NULL,'TOP SECRET');
                    INSERT INTO events VALUES ('t','created','2026-11-06 00:00:00'),('t','approve','2026-11-06 00:01:00');
                    INSERT INTO tasks VALUES ('completed'),('working');
                    INSERT INTO messages VALUES ('interrupted','2026-11-06');
                    INSERT INTO chat_turns VALUES ('uncertain');
                ''')
            result = trial.report(database, '2026-11-06')
            self.assertEqual(result['proposal_creation_cohort']['confirmation_rate'], 1/3)
            self.assertEqual(result['draft_creation_cohort']['adoption_rate_lower_bound'], 1)
            self.assertAlmostEqual(result['delegation_to_acceptance_seconds'][0], 60, delta=1)
            self.assertEqual(result['unfinished_tasks_now'], 1)
            self.assertEqual(result['failed_or_interrupted_messages'], 1)
            self.assertIsNone(result['model_timeouts_reported'])
            self.assertNotIn('TOP SECRET', json.dumps(result))
            empty = trial.report(database, '2026-11-07')
            self.assertIsNone(empty['proposal_creation_cohort']['confirmation_rate'])
            self.assertEqual(empty['delegations_created'], 0)
