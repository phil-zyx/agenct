"""Offline protocol tests; never execute repository code or start Docker."""
import copy
import importlib.util
import pathlib
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('runner', pathlib.Path(__file__).with_name('runner.py'))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class ProtocolTests(unittest.TestCase):
    def setUp(self):
        self.input = {'repository': 'repo', 'base_commit': 'a' * 40,
                      'bundle_base64': 'YWJj', 'patch': 'diff',
                      'checks': [{'name': 'unit', 'argv': ['cargo', 'test', '--offline']}]}

    def test_valid_input(self):
        self.assertEqual(runner.validate(self.input), b'abc')

    def test_duplicate_check_rejected(self):
        self.input['checks'].append(copy.deepcopy(self.input['checks'][0]))
        with self.assertRaises(ValueError):
            runner.validate(self.input)

    def test_bundle_must_be_valid_and_bounded(self):
        self.input['bundle_base64'] = 'not base64!'
        with self.assertRaises(ValueError):
            runner.validate(self.input)

    def test_paths_and_command_shapes_rejected(self):
        self.input['repository'] = '../../etc'
        with self.assertRaises(ValueError):
            runner.validate(self.input)
        self.input['repository'] = 'repo'
        self.input['checks'][0]['argv'] = 'sh -c anything'
        with self.assertRaises(ValueError):
            runner.validate(self.input)

    def test_unknown_fields_rejected(self):
        self.input['token'] = 'should not reach the worker'
        with self.assertRaises(ValueError):
            runner.validate(self.input)

    def test_entrypoint_never_runs_checks_or_claims_evidence(self):
        with patch('subprocess.run') as run, patch('builtins.print') as output:
            with self.assertRaisesRegex(RuntimeError, 'disabled'):
                runner.main()
            run.assert_not_called()
            output.assert_not_called()

    def test_git_index_attack_cannot_enable_evidence(self):
        # This is the concrete reviewed attack class, not an extra git diff workaround.
        self.input['checks'][0]['argv'] = [
            'sh', '-c', 'git update-index --assume-unchanged tracked; '
            'printf modified > tracked; git replace HEAD HEAD~1; true']
        runner.validate(self.input)
        with patch('sys.stdin') as stdin, patch('subprocess.run') as run:
            stdin.read.return_value = self.input
            with self.assertRaisesRegex(RuntimeError, 'disabled'):
                runner.main()
            run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
