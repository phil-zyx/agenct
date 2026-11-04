#!/usr/bin/env python3
"""Real Docker smoke for fixed benign fixtures, never a worker executor.

No credentials, external services, image pulls, arbitrary commands, or repository
source enter the container. The runtime's unconditional disablement is unchanged.
Requires a disposable Linux host with Docker, Python 3 and static C toolchain.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import independent_verifier as verifier


class FixtureObjects:
    length = 40

    def __init__(self):
        self.objects = {}

    def add(self, kind, body):
        oid = verifier.digest_object(kind, body, self.length)
        self.objects[oid] = (kind, body)
        return oid

    def read(self, oid):
        return self.objects[oid]


def fixture_snapshot():
    store = FixtureObjects()
    blob = store.add('blob', b'agenct benign fixture\n')
    tree = store.add('tree', b'100644 fixture.txt\x00' + bytes.fromhex(blob))
    commit = store.add('commit', ('tree ' + tree + '\n\nfixed fixture\n').encode())
    return verifier.extract_snapshot(store, commit)


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def command_with_diagnostics(args, env, timeout=60, check=True):
    try:
        return subprocess.run(args, env=env, stdin=subprocess.DEVNULL,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              timeout=timeout, check=check)
    except subprocess.CalledProcessError as error:
        # Only this fixed fixture harness uses this helper. No credentials are
        # passed, and stdout may be a binary archive: never dump it. JSON escaping
        # prevents control characters/newlines from becoming CI log commands.
        diagnostic = (error.stderr or b'')[:4096].decode('utf-8', errors='replace')
        print('fixture command stderr: ' + json.dumps(diagnostic), file=sys.stderr)
        raise


def cleanup(docker, names, tag, image_id):
    """Attempt every resource even after timeouts; fail on any uncertain outcome."""
    failures = []
    resources = [(name, ('rm', '--force', name), ('inspect', name)) for name in names]
    resources.extend((image, ('image', 'rm', '--force', image), ('image', 'inspect', image))
                     for image in (tag, image_id) if image)
    for name, removal, inspection in resources:
        # Removal and verification are separately guarded: even a timed-out
        # removal may have succeeded, but it must not skip later resources.
        for operation, args in (('remove', removal), ('verify', inspection)):
            try:
                result = docker(*args, check=False)
                if operation == 'verify':
                    error = result.stderr.decode('utf-8', errors='replace')
                    missing = ('No such object: ' + name in error or
                               'No such image: ' + name in error)
                    if result.returncode == 0 or not missing:
                        failures.append(name + ': absence not confirmed')
            except (OSError, subprocess.SubprocessError) as error:
                failures.append(name + ': ' + operation + ' ' + type(error).__name__)
    require(not failures, 'cleanup incomplete: ' + ', '.join(failures))


def run():
    names = []
    tag = 'agenct-benign-smoke:' + uuid.uuid4().hex
    with tempfile.TemporaryDirectory(prefix='agenct-docker-smoke-') as temporary:
        directory = Path(temporary)
        # Do not inherit proxy, credential, remote-daemon or Docker config values.
        env = {'PATH': os.environ['PATH'], 'HOME': temporary,
               'DOCKER_CONFIG': temporary, 'DOCKER_BUILDKIT': '1', 'LANG': 'C'}

        def command(args, timeout=60, check=True):
            return command_with_diagnostics(args, env, timeout, check)

        def docker(*args, **kwargs):
            return command(['docker', *args], **kwargs)

        def inspect(name):
            return json.loads(docker('inspect', name).stdout)[0]

        def create(plan, suffix):
            name = tag.replace(':', '-') + '-' + suffix
            names.append(name)  # Register before a possibly uncertain create result.
            docker('create', '--name=' + name, *plan.argv[1:])
            observation = inspect(name)
            require(observation['State']['Status'] == 'created', 'must not have started')
            # Reuse the existing policy validator for configuration before start;
            # only terminal-state fields are synthetic at this point, not evidence.
            configuration = dict(observation)
            configuration['State'] = {'Status': 'exited', 'Running': False,
                                      'ExitCode': 0, 'OOMKilled': False, 'Error': ''}
            verifier.validate_daemon_fixture(plan, configuration)
            return name

        snapshot = fixture_snapshot()
        image_id = None
        try:
            docker('version')
            command(['cc', '-static', '-O2', '-Wall', '-Wextra', '-Werror',
                     str(Path(__file__).with_name('probe.c')), '-o', str(directory / 'probe')])
            (directory / 'fixture.txt').write_bytes(snapshot.files[0].content)
            (directory / 'Dockerfile').write_text(
                'FROM scratch\n'
                'COPY --chmod=0555 probe /opt/agenct-checks/probe\n'
                'COPY --chmod=0555 probe /opt/agenct-checks/hang\n'
                'COPY --chmod=0444 fixture.txt /candidate/fixture.txt\n'
                'USER 65534:65534\nWORKDIR /candidate\n')
            docker('build', '--network=none', '--pull=false', '--no-cache',
                   '--tag=' + tag, str(directory), timeout=180)
            image = inspect(tag)
            image_id = image['Id']
            require(image['Config'].get('OnBuild') in (None, []), 'unexpected ONBUILD')
            require(image['Config'].get('Volumes') in (None, {}), 'unexpected volume')
            plan = verifier.plan_check(snapshot, image_id, 'probe')
            name = create(plan, 'check')
            archive = docker('cp', name + ':/candidate', '-').stdout
            verifier.verify_source_archive(snapshot, archive)
            docker('start', name)
            docker('wait', name, timeout=30)
            verifier.validate_daemon_fixture(plan, inspect(name))
            print('PASS: real stopped-container archive, immutable source, uid, tmpfs and daemon policy')

            hang = create(verifier.plan_check(snapshot, image_id, 'hang'), 'timeout')
            docker('start', hang)
            try:
                docker('wait', hang, timeout=2)
                raise RuntimeError('fixed sleep fixture ended before timeout')
            except subprocess.TimeoutExpired:
                docker('kill', hang)
            terminated = inspect(hang)['State']
            require(terminated['Running'] is False and terminated['ExitCode'] != 0,
                    'timeout fixture not terminated')
            print('PASS: bounded wait and explicit container termination')
        finally:
            # A failed cleanup fails the job. No broad daemon prune operations.
            cleanup(docker, names, tag, image_id)
        print('PASS: named fixture containers and image removed')
        print('LIMIT: benign Docker smoke only; production executor remains disabled')


if __name__ == '__main__':
    run()
