"""Offline lifecycle/rejection tests. No process, Docker or external service calls."""
import copy
import io
import tarfile
import unittest
from unittest.mock import patch

import staged_supervisor as stage


class Clock:
    def __init__(self):
        self.value = 0.0

    def now(self):
        return self.value

    def sleep(self, seconds):
        self.value += seconds


class FakeEngine:
    def __init__(self):
        self.started = False
        self.removed = False
        self.created = False
        self.keep_running = False
        self.fail_create = False
        self.fail_cleanup = False
        self.final_mutation = None
        self.source_mutation = False
        self.calls = []
        self.states = ['running', 'exited']
        self.exit_code = 0
        self.archive_count = 0

    def create(self, name, plan, labels, timeout):
        self.calls.append('create')
        self.name, self.plan, self.labels = name, plan, labels
        self.created = True
        if self.fail_create:
            raise TimeoutError('create response uncertain')

    def inspect(self, name, timeout):
        self.calls.append('inspect')
        state = 'created'
        if self.started:
            state = 'running' if self.keep_running else self.states.pop(0)
        obs = {'Id': 'c' * 64, 'Name': '/' + self.name, 'Image': self.plan.image_id,
               'Config': {'Image': self.plan.image_id, 'Labels': copy.deepcopy(self.labels),
                          'Entrypoint': [self.plan.checker],
                          'Cmd': ['--source=/candidate', '--output=/scratch'],
                          'User': '65534:65534', 'WorkingDir': '/candidate'},
               'RestartCount': 0,
                'HostConfig': {'RestartPolicy': {'Name': 'no', 'MaximumRetryCount': 0},
                               'ReadonlyRootfs': True, 'Privileged': False,
                              'NetworkMode': 'none', 'PidMode': '', 'IpcMode': 'private',
                              'CapDrop': ['ALL'], 'CapAdd': None, 'Binds': None, 'Devices': None,
                              'SecurityOpt': ['no-new-privileges'], 'PidsLimit': 64,
                              'Memory': 512 * 1024 * 1024, 'MemorySwap': 512 * 1024 * 1024,
                              'NanoCpus': 1_000_000_000, 'VolumesFrom': None, 'DeviceRequests': None,
                               'Tmpfs': {'/scratch': 'rw,nosuid,nodev,size=134217728,mode=1777',
                                         '/tmp': 'rw,nosuid,nodev,noexec,size=33554432,mode=1777'}},
               'Mounts': [{'Type': 'tmpfs', 'Destination': '/scratch'}],
               'State': {'Status': state, 'Running': state == 'running',
                         'ExitCode': self.exit_code, 'OOMKilled': False, 'Error': ''}}
        if self.started and self.final_mutation:
            self.final_mutation(obs)
        return obs

    def archive(self, container_id, timeout):
        self.calls.append('archive')
        self.archive_count += 1
        data = b'changed source' if self.source_mutation and self.started else b'agenct benign fixture\n'
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode='w') as archive:
            member = tarfile.TarInfo('candidate/fixture.txt')
            member.size = len(data)
            member.mode = 0o444
            archive.addfile(member, io.BytesIO(data))
        return out.getvalue()

    def start(self, container_id, timeout):
        self.calls.append('start')
        self.started = True

    def remove(self, name, timeout):
        self.calls.append('remove')
        self.removed = True
        if self.fail_cleanup:
            raise TimeoutError('daemon unreachable during cleanup')
        return True


class SupervisorTests(unittest.TestCase):
    def setUp(self):
        self.engine = FakeEngine()
        self.clock = Clock()
        self.image = 'sha256:' + 'b' * 64

    def run_stage(self, **kwargs):
        return stage.observe_benign(self.engine, self.image, monotonic=self.clock.now,
                                    sleep=self.clock.sleep, **kwargs)

    def test_success_requires_host_binding_two_archives_and_cleanup(self):
        observation = self.run_stage()
        self.assertEqual(observation.status, 'benign-stage-observed')
        self.assertFalse(observation.production_eligible)
        self.assertTrue(observation.cleanup_confirmed)
        self.assertTrue(self.engine.removed)
        self.assertEqual(self.engine.archive_count, 2)
        self.assertEqual(observation.snapshot_digest, stage.fixture_snapshot().digest)
        self.assertEqual(observation.candidate_commit, stage.fixture_snapshot().commit)
        self.assertEqual(self.engine.calls[-1], 'remove')

    def test_revoked_before_create_has_no_side_effects(self):
        with self.assertRaises(stage.StageRevoked):
            self.run_stage(still_authorized=lambda: False)
        self.assertEqual(self.engine.calls, [])

    def test_revoked_after_start_removes_without_evidence(self):
        with self.assertRaises(stage.StageRevoked):
            self.run_stage(still_authorized=lambda: not self.engine.started)
        self.assertTrue(self.engine.started)
        self.assertTrue(self.engine.removed)

    def test_timeout_cleans_running_container(self):
        self.engine.keep_running = True
        with self.assertRaises(stage.StageTimeout):
            self.run_stage(timeout=1)
        self.assertTrue(self.engine.started)
        self.assertTrue(self.engine.removed)
        self.assertLessEqual(self.clock.value, 1.1)

    def test_uncertain_creation_is_registered_for_cleanup(self):
        self.engine.fail_create = True
        with self.assertRaises(TimeoutError):
            self.run_stage()
        self.assertEqual(self.engine.calls, ['create', 'remove'])

    def test_cleanup_failure_prevents_success_observation(self):
        self.engine.fail_cleanup = True
        with self.assertRaises(stage.CleanupUnconfirmed):
            self.run_stage()

    def test_revoked_during_cleanup_prevents_success(self):
        with self.assertRaises(stage.StageRevoked):
            self.run_stage(still_authorized=lambda: not self.engine.removed)
        self.assertTrue(self.engine.removed)

    def test_identity_and_policy_changes_rejected(self):
        mutations = [lambda obs: obs.update(Id='d' * 64),
                     lambda obs: obs.update(Name='/different'),
                     lambda obs: obs.update(Image='sha256:' + 'a' * 64),
                     lambda obs: obs['Config']['Labels'].update({'agenct.snapshot': 'forged'}),
                     lambda obs: obs['HostConfig'].update(ReadonlyRootfs=False),
                     lambda obs: obs['HostConfig'].update(PidMode='host')]
        for mutate in mutations:
            self.engine = FakeEngine()
            self.engine.final_mutation = mutate
            with self.subTest(mutation=mutate), self.assertRaises((stage.StageRejected, stage.verifier.Rejected)):
                self.run_stage()
            self.assertTrue(self.engine.removed)

    def test_invalid_initial_tmpfs_never_starts_and_cleans_up(self):
        original_inspect = self.engine.inspect
        def weakened(*args, **kwargs):
            observed = original_inspect(*args, **kwargs)
            observed['HostConfig']['Tmpfs']['/tmp'] = 'rw,size=17179869184'
            return observed
        self.engine.inspect = weakened
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertNotIn('start', self.engine.calls)
        self.assertTrue(self.engine.removed)

    def test_tmpfs_change_after_start_prevents_observation(self):
        self.engine.final_mutation = lambda obs: obs['HostConfig'].pop('Tmpfs')
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertTrue(self.engine.started)
        self.assertTrue(self.engine.removed)

    def test_initial_restart_policy_rejected_before_start(self):
        original_inspect = self.engine.inspect
        def restarting(*args, **kwargs):
            observed = original_inspect(*args, **kwargs)
            observed['HostConfig']['RestartPolicy'] = {
                'Name': 'on-failure', 'MaximumRetryCount': 5}
            return observed
        self.engine.inspect = restarting
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertNotIn('start', self.engine.calls)
        self.assertTrue(self.engine.removed)

    def test_restart_between_polls_cannot_hide_failed_attempt(self):
        # A zero-exit replacement run must not hide an earlier failed attempt.
        self.engine.states = ['exited']
        self.engine.final_mutation = lambda obs: obs.update(RestartCount=1)
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertTrue(self.engine.started)
        self.assertTrue(self.engine.removed)

    def test_post_check_source_change_and_exit_failure_rejected(self):
        self.engine.source_mutation = True
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertTrue(self.engine.removed)
        self.engine = FakeEngine()
        self.engine.exit_code = 1
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertTrue(self.engine.removed)

    def test_prestarted_container_or_wrong_initial_source_never_starts_check(self):
        original_inspect = self.engine.inspect
        def prestarted(*args, **kwargs):
            observed = original_inspect(*args, **kwargs)
            observed['State'] = {'Status': 'running', 'Running': True}
            return observed
        self.engine.inspect = prestarted
        with self.assertRaises(stage.StageRejected):
            self.run_stage()
        self.assertNotIn('start', self.engine.calls)
        self.assertTrue(self.engine.removed)
        self.engine = FakeEngine()
        self.engine.archive = lambda *args, **kwargs: b'not an archive'
        with self.assertRaises(stage.verifier.Rejected):
            self.run_stage()
        self.assertNotIn('start', self.engine.calls)
        self.assertTrue(self.engine.removed)

    def test_only_fixed_checker_and_finite_deadline_allowed(self):
        for checker in ('shell', '../probe', 'cargo-test'):
            with self.assertRaises(stage.StageRejected):
                self.run_stage(checker=checker)
        for timeout in (0, -1, float('inf'), float('nan'), True, 61):
            with self.assertRaises(stage.StageRejected):
                self.run_stage(timeout=timeout)
        self.assertEqual(self.engine.calls, [])

    def test_no_flag_or_staging_receipt_enables_production(self):
        observed = self.run_stage()
        with patch('subprocess.run') as process:
            with self.assertRaises(stage.verifier.ExecutionDisabled):
                stage.execute_verified_job(observed, full_safety_gates_verified=True)
            process.assert_not_called()


if __name__ == '__main__':
    unittest.main()
