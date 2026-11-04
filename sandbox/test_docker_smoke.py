"""Validate the benign harness's immutable input without requiring Docker."""
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import Mock, patch
import contextlib
import io
import subprocess

spec = importlib.util.spec_from_file_location(
    'docker_smoke', Path(__file__).parent / 'docker_smoke' / 'run.py')
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class SmokeFixtureTests(unittest.TestCase):
    def test_input_is_deterministic_and_only_the_fixed_benign_file(self):
        snapshot = smoke.fixture_snapshot()
        self.assertEqual(snapshot, smoke.fixture_snapshot())
        self.assertEqual(len(snapshot.files), 1)
        self.assertEqual(snapshot.files[0].path, 'fixture.txt')
        self.assertEqual(snapshot.files[0].content, b'agenct benign fixture\n')
        self.assertFalse(snapshot.files[0].executable)

    def test_smoke_does_not_enable_execution(self):
        with self.assertRaises(smoke.verifier.ExecutionDisabled):
            smoke.verifier.execute(smoke.fixture_snapshot())

    def test_failed_runtime_checks_are_errors(self):
        with self.assertRaisesRegex(RuntimeError, 'failure'):
            smoke.require(False, 'failure')

    def test_cleanup_continues_after_remove_and_inspect_timeouts(self):
        calls = []

        def docker(*args, **kwargs):
            calls.append(args)
            if args == ('rm', '--force', 'first'):
                raise subprocess.TimeoutExpired('docker', 60)
            if args == ('inspect', 'first'):
                raise OSError('daemon unavailable')
            if 'inspect' in args:
                return subprocess.CompletedProcess(args, 1, b'',
                    ('Error: No such object: ' + args[-1]).encode())
            return subprocess.CompletedProcess(args, 0, b'', b'')

        with self.assertRaisesRegex(RuntimeError, 'cleanup incomplete'):
            smoke.cleanup(docker, ['first', 'second'], 'tag', 'image-id')
        self.assertIn(('rm', '--force', 'second'), calls)
        self.assertIn(('image', 'rm', '--force', 'tag'), calls)
        self.assertIn(('image', 'rm', '--force', 'image-id'), calls)
        self.assertIn(('image', 'inspect', 'image-id'), calls)
        self.assertEqual(len(calls), 8)

    def test_cleanup_rejects_daemon_error_as_absence_proof(self):
        docker = Mock(return_value=subprocess.CompletedProcess(
            [], 1, b'', b'Cannot connect to the Docker daemon'))
        with self.assertRaisesRegex(RuntimeError, 'absence not confirmed'):
            smoke.cleanup(docker, ['first'], 'tag', None)
        self.assertEqual(docker.call_count, 4)

    def test_cleanup_accepts_verified_missing_resources(self):
        def docker(*args, **kwargs):
            return subprocess.CompletedProcess(args, 1, b'',
                ('Error: No such object: ' + args[-1]).encode())
        smoke.cleanup(docker, ['first'], 'tag', 'image-id')

    def test_failed_command_emits_bounded_escaped_stderr_not_stdout(self):
        error = subprocess.CalledProcessError(125, ['docker', 'create'],
            output=b'never log binary stdout', stderr=b'bad pid\n::error::oops\x1b' + b'x' * 10000)
        output = io.StringIO()
        with patch.object(smoke.subprocess, 'run', side_effect=error), contextlib.redirect_stderr(output):
            with self.assertRaises(subprocess.CalledProcessError):
                smoke.command_with_diagnostics(['docker', 'create'], {})
        message = output.getvalue()
        self.assertIn('bad pid', message)
        self.assertNotIn('never log binary stdout', message)
        self.assertNotIn('\x1b', message)
        self.assertEqual(message.count('\n'), 1)
        self.assertLess(len(message), 4300)


if __name__ == '__main__':
    unittest.main()
