"""Offline adversarial object/planner tests. No Docker, shell or repository checks."""
import copy
import hashlib
import importlib.util
import io
import multiprocessing
import os
import pathlib
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch
import zlib

spec = importlib.util.spec_from_file_location('independent_verifier', pathlib.Path(__file__).with_name('independent_verifier.py'))
v = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = v
spec.loader.exec_module(v)


def _probe_fifo(directory, oid, result):
    try:
        v.LooseObjectStore(directory).read(oid)
        result.put('accepted')
    except v.Rejected:
        result.put('rejected')
    except Exception as error:
        result.put(type(error).__name__)


class Objects:
    def __init__(self, length=40):
        self.data = {}
        self.length = length

    def add(self, kind, body):
        oid = v.digest_object(kind, body, self.length)
        self.data[oid] = (kind, body)
        return oid

    def read(self, oid):
        return self.data[oid]

    def tree(self, entries):
        return self.add('tree', b''.join(mode + b' ' + name + b'\0' + bytes.fromhex(oid)
                                        for mode, name, oid in entries))

    def commit(self, tree):
        return self.add('commit', ('tree ' + tree + '\nauthor Test <test@example.test> 1 +0000\n'
                                  'committer Test <test@example.test> 1 +0000\n\nfixture\n').encode())


def fixture(length=40):
    store = Objects(length)
    blob = store.add('blob', b'original immutable source\n')
    tree = store.tree([(b'100644', b'source.txt', blob)])
    commit = store.commit(tree)
    return store, commit


class SnapshotTests(unittest.TestCase):
    def test_exact_git_sha1_and_sha256_content(self):
        for length in (40, 64):
            store, commit = fixture(length)
            snapshot = v.extract_snapshot(store, commit)
            self.assertEqual(snapshot.commit, commit)
            self.assertEqual(snapshot.files[0].content, b'original immutable source\n')
            self.assertEqual(snapshot, v.extract_snapshot(store, commit))

    def test_forged_object_cannot_keep_claimed_hash(self):
        store, commit = fixture()
        store.data[commit] = ('commit', b'tree ' + b'a' * 40 + b'\n')
        with self.assertRaisesRegex(v.Rejected, 'identity'):
            v.extract_snapshot(store, commit)

    def test_rejects_symlink_submodule_and_git_metadata(self):
        for mode, name in [(b'120000', b'link'), (b'160000', b'submodule'),
                           (b'100644', b'.git'), (b'100644', b'.GIT'),
                           (b'100644', b'../escape'), (b'100644', b'/root'),
                           (b'100644', b'win\\path'), (b'100644', b'drive:path')]:
            store = Objects()
            blob = store.add('blob', b'data')
            commit = store.commit(store.tree([(mode, name, blob)]))
            with self.subTest(mode=mode, name=name), self.assertRaises(v.Rejected):
                v.extract_snapshot(store, commit)

    def test_duplicate_tree_entries_rejected(self):
        store = Objects()
        blob = store.add('blob', b'data')
        tree = store.tree([(b'100644', b'x', blob), (b'100644', b'x', blob)])
        with self.assertRaisesRegex(v.Rejected, 'duplicate'):
            v.extract_snapshot(store, store.commit(tree))

    def test_index_and_replace_refs_are_never_consulted(self):
        store, commit = fixture()
        # These attack inputs are not part of the extraction interface at all.
        store.index = {'source.txt': 'assume-unchanged'}
        store.replace_refs = {commit: 'f' * 40}
        with patch('subprocess.run') as shell:
            snapshot = v.extract_snapshot(store, commit)
            shell.assert_not_called()
        self.assertEqual(snapshot.files[0].content, b'original immutable source\n')

    def test_loose_objects_verified_without_git_process(self):
        store, commit = fixture()
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            for oid, (kind, body) in store.data.items():
                path = root / oid[:2] / oid[2:]
                path.parent.mkdir(exist_ok=True)
                path.write_bytes(zlib.compress(kind.encode() + b' ' + str(len(body)).encode() + b'\0' + body))
            reader = v.LooseObjectStore(root)
            with patch('subprocess.run') as process:
                self.assertEqual(v.extract_snapshot(reader, commit), v.extract_snapshot(store, commit))
                process.assert_not_called()
            object_path = root / commit[:2] / commit[2:]
            object_path.unlink()
            object_path.symlink_to('/etc/passwd')
            with self.assertRaises(v.Rejected):
                reader.read(commit)

    def test_fifo_is_rejected_without_blocking_before_fstat(self):
        with tempfile.TemporaryDirectory() as directory:
            oid = 'a' * 40
            path = pathlib.Path(directory) / oid[:2] / oid[2:]
            path.parent.mkdir()
            os.mkfifo(path)
            # Isolate the probe so this regression fails rather than hanging CI
            # if a future change accidentally drops O_NONBLOCK.
            context = multiprocessing.get_context('fork')
            result = context.Queue()
            process = context.Process(target=_probe_fifo, args=(directory, oid, result))
            process.start()
            process.join(timeout=2)
            hung = process.is_alive()
            if hung:
                process.terminate()
                process.join(timeout=2)
            try:
                self.assertFalse(hung, 'opening a FIFO blocked before regular-file validation')
                self.assertEqual(process.exitcode, 0)
                self.assertEqual(result.get(timeout=1), 'rejected')
            finally:
                result.close()
                result.join_thread()

    def test_decompression_bomb_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            oid = 'a' * 40
            path = pathlib.Path(directory) / oid[:2] / oid[2:]
            path.parent.mkdir()
            path.write_bytes(zlib.compress(b'x' * 2048))
            with patch.object(v, 'MAX_OBJECT', 64), self.assertRaises(v.Rejected):
                v.LooseObjectStore(directory).read(oid)

    def test_tree_traversal_budget_covers_empty_subtrees(self):
        store = Objects()
        empty = store.tree([])
        root = store.tree([(b'40000', str(i).encode(), empty) for i in range(5)])
        with patch.object(v, 'MAX_FILES', 1), self.assertRaises(v.Rejected):
            v.extract_snapshot(store, store.commit(root))


class PlanningTests(unittest.TestCase):
    def setUp(self):
        store, commit = fixture()
        self.snapshot = v.extract_snapshot(store, commit)
        self.image = 'sha256:' + 'b' * 64
        self.plan = v.plan_check(self.snapshot, self.image, 'unit')

    def test_build_context_does_not_execute_repository_dockerfile(self):
        base = 'trusted/toolchain@sha256:' + 'a' * 64
        context = v.source_image_context(self.snapshot, base)
        self.assertEqual(context, v.source_image_context(self.snapshot, base))
        with tarfile.open(fileobj=io.BytesIO(context)) as archive:
            dockerfile = archive.extractfile('Dockerfile').read()
            self.assertNotIn(b'RUN ', dockerfile)
            self.assertIn(b'COPY snapshot/ /candidate/', dockerfile)
            self.assertEqual(archive.getmember('snapshot/source.txt').mode, 0o444)
            self.assertTrue(all(not item.issym() and not item.islnk() for item in archive.getmembers()))

    def test_manually_forged_snapshot_cannot_escape_archive(self):
        bad = v.Snapshot(self.snapshot.commit, self.snapshot.tree,
                         (v.SnapshotFile('../../Dockerfile', False, b'RUN dangerous'),), 'forged')
        with self.assertRaises(v.Rejected):
            v.source_image_context(bad, 'trusted@sha256:' + 'a' * 64)

    def test_container_plan_uses_only_readonly_candidate_image(self):
        args = self.plan.argv
        for flag in ('--read-only', '--network=none', '--cap-drop=ALL', '--user=65534:65534'):
            self.assertIn(flag, args)
        self.assertFalse(any(arg.startswith(('--mount', '--volume', '-v=')) for arg in args))
        self.assertNotIn('--tmpfs=/candidate', args)
        self.assertEqual(self.plan.checker, '/opt/agenct-checks/unit')

    def observation(self):
        return {'Image': self.image,
                'Config': {'Image': self.image, 'Entrypoint': [self.plan.checker],
                           'Cmd': ['--source=/candidate', '--output=/scratch'],
                           'User': '65534:65534', 'WorkingDir': '/candidate'},
                'HostConfig': {'ReadonlyRootfs': True, 'Privileged': False,
                               'NetworkMode': 'none', 'PidMode': '', 'IpcMode': 'private', 'CapDrop': ['ALL'],
                               'CapAdd': None, 'Binds': None, 'Devices': None,
                               'SecurityOpt': ['no-new-privileges'], 'PidsLimit': 64,
                               'Memory': 512 * 1024 * 1024, 'MemorySwap': 512 * 1024 * 1024,
                               'NanoCpus': 1_000_000_000, 'VolumesFrom': None, 'DeviceRequests': None},
                'Mounts': [{'Type': 'tmpfs', 'Destination': '/scratch'}],
                'State': {'Status': 'exited', 'Running': False, 'ExitCode': 0,
                          'OOMKilled': False, 'Error': ''}}

    def test_observation_output_is_explicitly_offline_only(self):
        observed = v.validate_daemon_fixture(self.plan, self.observation())
        self.assertEqual(observed['status'], 'offline-fixture-only')
        self.assertNotIn('passed', observed)

    def test_wrong_image_or_writable_candidate_rejected(self):
        observation = self.observation()
        observation['Image'] = 'sha256:' + 'c' * 64
        with self.assertRaises(v.Rejected):
            v.validate_daemon_fixture(self.plan, observation)
        observation = self.observation()
        observation['Mounts'].append({'Type': 'tmpfs', 'Destination': '/candidate'})
        with self.assertRaises(v.Rejected):
            v.validate_daemon_fixture(self.plan, observation)
        observation = self.observation()
        observation['HostConfig']['ReadonlyRootfs'] = False
        with self.assertRaises(v.Rejected):
            v.validate_daemon_fixture(self.plan, observation)

    def test_pid_and_ipc_host_or_container_namespaces_rejected(self):
        for field in ('PidMode', 'IpcMode'):
            for mode in ('host', 'container:another-worker', 'shareable', None):
                observation = self.observation()
                observation['HostConfig'][field] = mode
                with self.subTest(field=field, mode=mode), self.assertRaises(v.Rejected):
                    v.validate_daemon_fixture(self.plan, observation)
            observation = self.observation()
            del observation['HostConfig'][field]
            with self.assertRaises(v.Rejected):
                v.validate_daemon_fixture(self.plan, observation)
            for mode in ('', 'private'):
                observation = self.observation()
                observation['HostConfig'][field] = mode
                self.assertEqual(v.validate_daemon_fixture(self.plan, observation)['status'],
                                 'offline-fixture-only')
        self.assertIn('--pid=private', self.plan.argv)
        self.assertIn('--ipc=private', self.plan.argv)

    def test_test_stdout_cannot_be_used_as_daemon_evidence(self):
        fake = {'passed': True, 'commit': self.snapshot.commit, 'image': self.image}
        with self.assertRaises(v.Rejected):
            v.validate_daemon_fixture(self.plan, fake)

    def source_archive(self, files=None):
        files = files if files is not None else [('candidate/source.txt', b'original immutable source\n', 0o444)]
        out = io.BytesIO()
        with tarfile.open(fileobj=out, mode='w') as archive:
            for name, content, mode in files:
                member = tarfile.TarInfo(name)
                member.size = len(content)
                member.mode = mode
                archive.addfile(member, io.BytesIO(content))
        return out.getvalue()

    def test_stopped_image_source_matches_exact_snapshot(self):
        digest = v.verify_source_archive(self.snapshot, self.source_archive())
        self.assertEqual(digest, self.snapshot.digest)

    def test_archive_extra_mutated_missing_and_writable_files_rejected(self):
        cases = [[], [('candidate/source.txt', b'mutated', 0o444)],
                 [('candidate/source.txt', b'original immutable source\n', 0o644)],
                 [('candidate/source.txt', b'original immutable source\n', 0o444),
                  ('candidate/extra', b'base-image contamination', 0o444)],
                 [('../escape', b'bad', 0o444)]]
        for files in cases:
            with self.subTest(files=files), self.assertRaises(v.Rejected):
                v.verify_source_archive(self.snapshot, self.source_archive(files))

    def test_source_archive_symlink_and_hardlink_rejected(self):
        for kind in (tarfile.SYMTYPE, tarfile.LNKTYPE):
            out = io.BytesIO()
            with tarfile.open(fileobj=out, mode='w') as archive:
                member = tarfile.TarInfo('candidate/source.txt')
                member.type = kind
                member.linkname = '/etc/passwd'
                archive.addfile(member)
            with self.assertRaises(v.Rejected):
                v.verify_source_archive(self.snapshot, out.getvalue())

    def test_base_image_cannot_run_onbuild_or_mount_source_volume(self):
        base = 'trusted/toolchain@sha256:' + 'a' * 64
        valid = {'RepoDigests': [base], 'Config': {'OnBuild': None, 'Volumes': None}}
        v.validate_base_fixture(base, valid)
        for key, value in [('OnBuild', ['RUN dangerous']), ('Volumes', {'/candidate': {}})]:
            bad = copy.deepcopy(valid)
            bad['Config'][key] = value
            with self.assertRaises(v.Rejected):
                v.validate_base_fixture(base, bad)

    def test_execution_is_hard_disabled(self):
        with patch('subprocess.run') as process:
            with self.assertRaises(v.ExecutionDisabled):
                v.execute(self.plan)
            process.assert_not_called()


if __name__ == '__main__':
    unittest.main()
