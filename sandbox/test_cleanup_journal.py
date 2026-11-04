"""Offline durable lifecycle tests; no Docker or external services."""
from pathlib import Path
import multiprocessing
import os
import signal
import tempfile
import unittest

import cleanup_journal as journal
import staged_supervisor as stage
from test_staged_supervisor import Clock, FakeEngine


class RecoverableEngine(FakeEngine):
    def lookup(self, identity, timeout):
        self.calls.append('lookup')
        if not self.created or self.removed:
            return None
        # Recovery observes identity, not process exit or test output.
        was_started = self.started
        self.started = False
        try:
            observed = self.inspect(identity, timeout)
        finally:
            self.started = was_started
        if self.final_mutation:
            self.final_mutation(observed)
        return observed


class JournalTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.path = Path(self.temp.name) / 'journal'
        self.engine = RecoverableEngine()
        self.image = 'sha256:' + 'b' * 64
        self.snapshot = stage.fixture_snapshot()
        self.name = 'agenct-stage-' + 'a' * 32

    def register_created(self, durable, bind=False):
        durable.register(self.name, self.image, self.snapshot.digest)
        plan = stage.verifier.plan_check(self.snapshot, self.image, 'probe')
        self.engine.create(self.name, plan, {'agenct.stage': self.name,
                           'agenct.snapshot': self.snapshot.digest}, timeout=5)
        if bind:
            durable.created(self.name)
            durable.bind(self.name, 'c' * 64)

    def test_success_removes_record_and_container(self):
        clock = Clock()
        with journal.CleanupJournal(self.path) as durable:
            receipt = stage.observe_benign(self.engine, self.image, journal=durable,
                                          monotonic=clock.now, sleep=clock.sleep)
            self.assertFalse(receipt.production_eligible)
            self.assertTrue(self.engine.removed)
            self.assertEqual(durable.pending(), [])

    def test_crash_before_and_after_id_binding_recovers_without_start(self):
        for bind in (False, True):
            self.engine = RecoverableEngine()
            with journal.CleanupJournal(self.path) as durable:
                self.register_created(durable, bind)
            with journal.CleanupJournal(self.path) as reopened:
                self.assertEqual(len(reopened.pending()), 1)
                reopened.reconcile(self.engine)
                self.assertEqual(reopened.pending(), [])
            self.assertTrue(self.engine.removed)
            self.assertNotIn('start', self.engine.calls)

    def test_crash_before_create_remains_pending_and_blocks_new_work(self):
        with journal.CleanupJournal(self.path) as durable:
            durable.register(self.name, self.image, self.snapshot.digest)
        with journal.CleanupJournal(self.path) as reopened:
            with self.assertRaises(journal.CleanupPending):
                reopened.reconcile(self.engine)
            with self.assertRaises(journal.CleanupPending):
                reopened.register('agenct-stage-' + 'd' * 32, self.image, self.snapshot.digest)
            self.assertEqual(len(reopened.pending()), 1)
        self.assertNotIn('remove', self.engine.calls)

    def test_uncertain_create_reconciles_only_when_matching_resource_visible(self):
        self.engine.fail_create = True
        with journal.CleanupJournal(self.path) as durable:
            with self.assertRaises(TimeoutError):
                stage.observe_benign(self.engine, self.image, journal=durable)
            self.assertEqual(durable.pending(), [])
            self.assertTrue(self.engine.removed)

    def test_mismatched_identity_never_removed(self):
        mutations = [lambda x: x.update(Id='d'*64), lambda x: x.update(Name='/other'),
                     lambda x: x.update(Image='sha256:'+'e'*64),
                     lambda x: x['Config']['Labels'].update({'agenct.stage': 'other'}),
                     lambda x: x['Config']['Labels'].update({'agenct.snapshot': 'wrong'})]
        for index, mutate in enumerate(mutations):
            self.engine = RecoverableEngine()
            with journal.CleanupJournal(Path(self.temp.name) / ('case' + str(index))) as durable:
                self.register_created(durable, bind=True)
                self.engine.final_mutation = mutate
                with self.assertRaises(journal.CleanupPending):
                    durable.reconcile(self.engine)
                self.assertFalse(self.engine.removed)
                self.assertEqual(len(durable.pending()), 1)

    def test_uncertain_remove_persists_then_absence_reconciles(self):
        with journal.CleanupJournal(self.path) as durable:
            self.register_created(durable, bind=True)
            self.engine.fail_cleanup = True
            with self.assertRaises(journal.CleanupPending):
                durable.reconcile(self.engine)
        with journal.CleanupJournal(self.path) as durable:
            self.assertEqual(len(durable.pending()), 1)
            durable.reconcile(self.engine)
            self.assertEqual(durable.pending(), [])

    def test_unreachable_daemon_preserves_record(self):
        with journal.CleanupJournal(self.path) as durable:
            self.register_created(durable, bind=True)
            self.engine.lookup = lambda *a, **k: (_ for _ in ()).throw(TimeoutError())
            with self.assertRaises(journal.CleanupPending):
                durable.reconcile(self.engine)
            self.assertEqual(len(durable.pending()), 1)
            self.assertNotIn('remove', self.engine.calls)

    def test_confirmed_create_missing_and_removed_before_journal_commit_reconcile(self):
        with journal.CleanupJournal(self.path) as durable:
            self.register_created(durable)
            durable.created(self.name)
            self.engine.removed = True
        with journal.CleanupJournal(self.path) as durable:
            durable.reconcile(self.engine)
            self.assertEqual(durable.pending(), [])

    def test_remove_success_without_actual_absence_stays_pending(self):
        with journal.CleanupJournal(self.path) as durable:
            self.register_created(durable, bind=True)
            self.engine.remove = lambda *a, **k: True
            with self.assertRaises(journal.CleanupPending):
                durable.reconcile(self.engine)
            self.assertEqual(len(durable.pending()), 1)

    def test_registration_delay_or_revocation_never_creates_container(self):
        for revoked in (False, True):
            clock = Clock()
            authorized = [True]
            with journal.CleanupJournal(self.path) as durable:
                original = durable.register
                def delayed(*args):
                    original(*args)
                    if revoked:
                        authorized[0] = False
                    else:
                        clock.sleep(31)
                durable.register = delayed
                expected = stage.StageRevoked if revoked else stage.StageTimeout
                with self.assertRaises(expected):
                    stage.observe_benign(self.engine, self.image, journal=durable,
                                         monotonic=clock.now, sleep=clock.sleep,
                                         still_authorized=lambda: authorized[0])
                self.assertEqual(self.engine.calls, [])
                self.assertEqual(durable.pending(), [])

    def test_cancel_unissued_rejects_created_or_bound_intents(self):
        with journal.CleanupJournal(self.path) as durable:
            self.register_created(durable, bind=True)
            with self.assertRaises(journal.JournalRejected):
                durable.cancel_unissued(self.name)
            self.assertEqual(len(durable.pending()), 1)

    def test_journal_registration_failure_never_calls_create(self):
        with journal.CleanupJournal(self.path) as durable:
            durable.register(self.name, self.image, self.snapshot.digest)
            with self.assertRaises(journal.CleanupPending):
                stage.observe_benign(self.engine, self.image, journal=durable)
            self.assertEqual(self.engine.calls, [])

    def test_sigkill_releases_lock_and_preserves_committed_intent(self):
        def killed_writer():
            with journal.CleanupJournal(self.path) as durable:
                durable.register(self.name, self.image, self.snapshot.digest)
                durable.created(self.name)
                durable.bind(self.name, 'c' * 64)
                os.kill(os.getpid(), signal.SIGKILL)
        process = multiprocessing.get_context('fork').Process(target=killed_writer)
        process.start()
        process.join(timeout=5)
        if process.is_alive():
            process.kill()
            process.join(timeout=5)
            self.fail('journal writer failed to finish within deadline')
        self.assertEqual(process.exitcode, -signal.SIGKILL)
        plan = stage.verifier.plan_check(self.snapshot, self.image, 'probe')
        self.engine.create(self.name, plan, {'agenct.stage': self.name,
                           'agenct.snapshot': self.snapshot.digest}, timeout=5)
        with journal.CleanupJournal(self.path) as durable:
            self.assertEqual(len(durable.pending()), 1)
            durable.reconcile(self.engine)
            self.assertEqual(durable.pending(), [])
        self.assertTrue(self.engine.removed)
        self.assertNotIn('start', self.engine.calls)

    def test_registration_storage_fault_prevents_create(self):
        with journal.CleanupJournal(self.path) as durable:
            def storage_fault(*_args):
                raise OSError('simulated storage write failure')
            durable.register = storage_fault
            with self.assertRaises(OSError):
                stage.observe_benign(self.engine, self.image, journal=durable)
            self.assertEqual(self.engine.calls, [])

    def test_lock_rejects_concurrent_supervisor(self):
        with journal.CleanupJournal(self.path):
            with self.assertRaises(journal.JournalRejected):
                journal.CleanupJournal(self.path)
        with journal.CleanupJournal(self.path):
            pass

    def test_corrupt_rows_and_corrupt_database_fail_before_daemon_calls(self):
        with journal.CleanupJournal(self.path) as durable:
            durable.connection.execute("INSERT INTO cleanup VALUES('foreign','bad','bad',NULL,0)")
            durable.connection.commit()
        with self.assertRaises(journal.JournalRejected):
            journal.CleanupJournal(self.path)
        self.assertEqual(self.engine.calls, [])
        (self.path / 'cleanup.sqlite3').write_bytes(b'corrupt database')
        with self.assertRaises(Exception):
            journal.CleanupJournal(self.path)

    def test_symlink_and_shared_directory_rejected(self):
        self.path.mkdir(mode=0o755)
        with self.assertRaises(journal.JournalRejected):
            journal.CleanupJournal(self.path)
        self.path.chmod(0o700)
        (self.path / 'cleanup.sqlite3').symlink_to(Path(self.temp.name) / 'other')
        with self.assertRaises(journal.JournalRejected):
            journal.CleanupJournal(self.path)


if __name__ == '__main__':
    unittest.main()
