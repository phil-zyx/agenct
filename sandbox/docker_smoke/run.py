#!/usr/bin/env python3
"""Real Docker smoke for fixed benign fixtures, never a worker executor.

No credentials, external services, image pulls, arbitrary commands, or repository
source enter the container. The runtime's unconditional disablement is unchanged.
Requires a disposable Linux host with Docker, Python 3 and static C toolchain.
"""
import argparse
import hashlib
import time
from contextlib import ExitStack
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import independent_verifier as verifier
import staged_supervisor as supervisor
from cleanup_journal import CleanupJournal


def fixture_snapshot():
    return supervisor.fixture_snapshot()


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


def run(journal_directory=None, report_file=None):
    names = []
    tag = 'agenct-benign-smoke:' + uuid.uuid4().hex
    with ExitStack() as stack:
        temporary = stack.enter_context(tempfile.TemporaryDirectory(prefix='agenct-docker-smoke-'))
        journal = (stack.enter_context(CleanupJournal(journal_directory))
                   if journal_directory is not None else None)
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

        class BenignEngine:
            """Trusted CLI adapter restricted to this fixed fixture harness."""
            def __init__(self):
                self.started = False

            def call(self, *args, timeout):
                try:
                    return docker(*args, timeout=timeout)
                except subprocess.TimeoutExpired as error:
                    raise supervisor.StageTimeout('bounded daemon operation timed out') from error

            def create(self, name, plan, labels, timeout):
                if journal is None:
                    names.append(name)
                self.call('create', '--name=' + name,
                       *('--label=' + key + '=' + value for key, value in labels.items()),
                       *plan.argv[1:], timeout=timeout)

            def inspect(self, name, timeout):
                observed = json.loads(self.call('inspect', name, timeout=timeout).stdout)[0]
                state = observed.get('State', {})
                if state.get('Status') == 'exited' and state.get('ExitCode') != 0 and not state.get('OOMKilled'):
                    print('benign fixture exit: ' + str(state.get('ExitCode')), file=sys.stderr)
                    diagnostic = docker('logs', name, timeout=timeout, check=False).stderr[:4096]
                    print('fixed probe diagnostic: ' + json.dumps(diagnostic.decode('utf-8', errors='replace')), file=sys.stderr)
                return observed

            def lookup(self, identity, timeout):
                result = docker('inspect', identity, timeout=timeout, check=False)
                if result.returncode == 0:
                    return json.loads(result.stdout)[0]
                # Unknown daemon failures are never interpreted as absence.
                missing = ('Error: No such object: ' + identity).encode()
                if result.returncode == 1 and result.stderr.strip() == missing:
                    return None
                raise RuntimeError('daemon absence not confirmed')

            def archive(self, container_id, timeout):
                return self.call('cp', container_id + ':/candidate', '-', timeout=timeout).stdout

            def start(self, container_id, timeout):
                self.call('start', container_id, timeout=timeout)
                self.started = True

            def remove(self, name, timeout):
                def bounded(*args, **kwargs):
                    return docker(*args, timeout=timeout, **kwargs)
                cleanup(bounded, [name], None, None)
                return True

        if journal is not None:
            journal.reconcile(BenignEngine())
        snapshot = fixture_snapshot()
        image_id = None
        try:
            docker('version')
            command(['cc', '-static', '-O2', '-Wall', '-Wextra', '-Werror',
                     str(Path(__file__).with_name('probe.c')), '-o', str(directory / 'probe')])
            (directory / 'fixture.txt').write_bytes(snapshot.files[0].content)
            (directory / 'Dockerfile').write_text(
                'FROM scratch\n'
                'WORKDIR /candidate\n'
                'COPY --chmod=0555 probe /opt/agenct-checks/probe\n'
                'COPY --chmod=0555 probe /opt/agenct-checks/hang\n'
                'COPY --chmod=0555 probe /opt/agenct-checks/memory\n'
                'COPY --chmod=0444 fixture.txt /candidate/fixture.txt\n'
                'USER 65534:65534\nWORKDIR /candidate\n')
            docker('build', '--network=none', '--pull=false', '--no-cache',
                   '--tag=' + tag, str(directory), timeout=180)
            image = inspect(tag)
            image_id = image['Id']
            require(image['Config'].get('OnBuild') in (None, []), 'unexpected ONBUILD')
            require(image['Config'].get('Volumes') in (None, {}), 'unexpected volume')
            observed = supervisor.observe_benign(BenignEngine(), image_id, journal=journal)
            require(observed.production_eligible is False and observed.cleanup_confirmed,
                    'staging observation must not enable production')
            require(observed.snapshot_digest == snapshot.digest,
                    'host-side snapshot binding changed')
            print('PASS: staged supervisor inspected immutable source and exact container/image before and after check')
            print('PASS: fixed probe observed outbound route/raw-socket denial, root/source write denial and tmp noexec')
            print('PASS: fixed probe enforced tmpfs/PID quotas and observed CPU throttling; cgroup memory/swap limits pinned')

            limited = supervisor.observe_benign(BenignEngine(), image_id, 'memory', journal=journal)
            require(not limited.production_eligible and limited.cleanup_confirmed, 'memory probe cleanup required')
            print('PASS: fixed allocation hit real cgroup memory limit (daemon exit 137 + OOMKilled), cleanup confirmed')

            timed = BenignEngine()
            try:
                supervisor.observe_benign(timed, image_id, 'hang', timeout=4, journal=journal)
                raise RuntimeError('fixed sleep fixture ended before timeout')
            except supervisor.StageTimeout:
                pass
            require(timed.started, 'deadline fixture must have started before timeout')
            print('PASS: supervised deadline terminated fixed fixture and confirmed cleanup')

            revoked = BenignEngine()
            try:
                supervisor.observe_benign(revoked, image_id, 'hang', journal=journal,
                                          still_authorized=lambda: not revoked.started)
                raise RuntimeError('revoked fixture produced an observation')
            except supervisor.StageRevoked:
                pass
            require(revoked.started, 'revocation fixture must have started first')
            print('PASS: revocation after start produced no evidence and confirmed cleanup')

        finally:
            # A failed cleanup fails the job. No broad daemon prune operations.
            cleanup(docker, names, tag, image_id)
        print('PASS: named fixture containers and image removed')
        print('LIMIT: benign Docker smoke only; production executor remains disabled')
        if report_file is not None:
            report = {'version': 1, 'scope': 'fixed-readonly-context-boundary',
                      'production_execution_enabled': False, 'checked_at': int(time.time()),
                      'probe_sha256': hashlib.sha256(Path(__file__).with_name('probe.c').read_bytes()).hexdigest(),
                      'checks': ['network', 'filesystem', 'pid', 'cpu', 'memory', 'timeout', 'revocation', 'cleanup']}
            fd = os.open(report_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
            with os.fdopen(fd, 'w') as stream:
                json.dump(report, stream, sort_keys=True)
                stream.write('\n')
                stream.flush()
                os.fsync(stream.fileno())



if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--journal-directory', help='private durable host directory for fixture cleanup')
    parser.add_argument('--report-file', help='new owner-only local evidence file; never authorizes worker execution')
    args = parser.parse_args()
    run(args.journal_directory, args.report_file)
