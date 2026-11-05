"""Offline tests of the smoke fixture. These never bind or connect a socket."""
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import uuid

spec = importlib.util.spec_from_file_location("bridge_smoke", Path(__file__).with_name("smoke-hub-adapter.py"))
smoke = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke)


class HubAdapterSmokeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        self.observations = self.directory / "calls.jsonl"
        self.runner = smoke.FixtureRunner(self.observations)
        self.turn = {"session_id": str(uuid.uuid4()), "history": [], "message": smoke.ALICE_PRIVATE,
                     "runtime_profile": smoke.route()}
        # Fail visibly if any supposedly offline fixture test opens a socket,
        # launches a child, or starts the real SDK/runner.
        for target in ("socket.socket", "subprocess.Popen"):
            guard = patch(target, side_effect=AssertionError("offline test attempted external execution"))
            guard.start()
            self.addCleanup(guard.stop)
        for target in ("run_sdk", "DshRunner"):
            guard = patch.object(smoke.adapter, target, side_effect=AssertionError("fixture used real runtime"))
            guard.start()
            self.addCleanup(guard.stop)

    def calls(self):
        return [json.loads(line) for line in self.observations.read_text().splitlines()]

    def test_child_environment_drops_all_inherited_credentials_and_configuration(self):
        inherited = {"DEEPSEEK_API_KEY": "test-only-key", "OPENAI_API_KEY": "test-only-key",
                     "AWS_SECRET_ACCESS_KEY": "test-only-key", "AGENCT_RUNTIME": "mock",
                     "AGENCT_DSH_STATE_DIR": "/must-not-be-used", "HTTP_PROXY": "http://invalid",
                     "AGENCT_OIDC_CONFIG_FILE": "/must-not-be-used", "PYTHONPATH": "/must-not-be-used",
                     "PATH": "/must-not-be-used", "HOME": "/must-not-be-used"}
        with patch.dict(os.environ, inherited):
            env = smoke.isolated_environment(self.directory)
        self.assertEqual(env, {"PATH": os.defpath, "LANG": "C.UTF-8",
                               "HOME": str(self.directory), "TMPDIR": str(self.directory)})

    def test_success_uses_production_parser_and_private_observation_file(self):
        with patch.object(smoke.adapter, "parse_model_response", wraps=smoke.adapter.parse_model_response) as parse:
            result = self.runner(self.turn)
        parse.assert_called_once()
        self.assertEqual(parse.call_args.args[1], smoke.ALICE_PRIVATE)
        self.assertEqual(result["backend"], "dsh")
        self.assertIsNone(result["proposal"])
        self.assertIn("TEST FIXTURE ONLY, no model", result["reply"])
        self.assertEqual(self.calls(), [self.turn])
        self.assertEqual(self.observations.stat().st_mode & 0o777, 0o600)

    def test_proposal_is_scoped_to_current_message_not_history(self):
        turn = self.turn | {"message": smoke.PROPOSAL_MESSAGE,
                            "history": [{"role": "user", "content": smoke.ALICE_PRIVATE}]}
        result = self.runner(turn)
        self.assertEqual(result["proposal"], {"owner": "bob", "summary": smoke.PROPOSAL_SUMMARY})
        self.assertNotIn(smoke.ALICE_PRIVATE, json.dumps(result))

    def test_faults_are_recorded_before_failure_and_across_fixture_restart(self):
        failed = self.turn | {"message": smoke.FAILURE_MESSAGE}
        with self.assertRaisesRegex(RuntimeError, "test-only failure"):
            self.runner(failed)
        restarted = smoke.FixtureRunner(self.observations)
        restarted(self.turn)
        self.assertEqual(self.calls(), [failed, self.turn])

    def test_malformed_negative_fixture_intentionally_bypasses_parser(self):
        with patch.object(smoke.adapter, "parse_model_response") as parse:
            result = self.runner(self.turn | {"message": smoke.MALFORMED_MESSAGE})
        parse.assert_not_called()
        self.assertEqual(set(result), {"reply"})
        self.assertEqual(len(self.calls()), 1)

    def test_non_loopback_urls_are_rejected_without_network(self):
        for base in ("https://127.0.0.1:80", "http://example.invalid", "http://localhost:80",
                     "http://127.0.0.1:80/path", "http://user@127.0.0.1:80",
                     "http://127.0.0.1:80?token=x", "http://127.0.0.1:80#fragment"):
            with self.subTest(base=base), self.assertRaises(AssertionError):
                smoke.request(base, "/api/me")

    def test_generated_identity_configuration_is_private_and_synthetic(self):
        with patch.object(smoke, "free_port", return_value=8080):
            bridge = smoke.BridgeSmoke(self.directory)
        self.assertEqual(len(set([bridge.adapter_token, *bridge.tokens.values()])), 3)
        config = json.loads((bridge.state / "config.json").read_text())
        self.assertEqual(config["runtime"], "dsh")
        self.assertEqual([user["id"] for user in config["users"]], ["alice", "bob"])
        for token in bridge.tokens.values():
            self.assertNotIn(token, json.dumps(config))
        self.assertEqual((self.directory / "fixture.json").stat().st_mode & 0o777, 0o600)
        self.assertEqual(bridge.histories, {"alice": [], "bob": []})
        self.assertIsNone(bridge.hub_process)
        self.assertIsNone(bridge.adapter_process)

    def test_startup_and_shutdown_fail_closed_without_waiting_on_real_processes(self):
        from unittest.mock import Mock
        process = Mock()
        process.poll.return_value = 1
        with self.assertRaisesRegex(RuntimeError, "exited during startup"):
            smoke.wait_ready(process, lambda: True, "fixture")
        process.returncode = 1
        with self.assertRaisesRegex(AssertionError, "exited unsuccessfully"):
            smoke.stop(process)
        process.wait.assert_called_once()


if __name__ == "__main__":
    unittest.main()
