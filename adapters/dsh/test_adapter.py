import http.client
import json
from pathlib import Path
import subprocess
import threading
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import uuid

from adapter import DshRunner, make_server, run_sdk, validate_turn, parse_model_response


def turn():
    return {"session_id": str(uuid.uuid4()), "history": [], "message": "Hello"}


class BoundaryTests(unittest.TestCase):
    def test_valid_turn(self):
        self.assertEqual(validate_turn(turn())["message"], "Hello")

    def test_reject_paths_system_roles_and_extra_fields(self):
        for changes in ({"session_id": "../../other"}, {"history": [{"role": "system", "content": "escape"}]},
                        {"message": " "}, {"owner_id": "someone"}, {"history": [None]},
                        {"message": "x" * 8001}, {"session_id": None}):
            with self.subTest(changes=changes), self.assertRaises((ValueError, TypeError)):
                validate_turn(turn() | changes)

    def test_sdk_profile_history_command_routing_and_cleanup(self):
        calls = []
        class Fake:
            def __init__(self, **config): calls.append(config)
            def __enter__(self): return self
            def __exit__(self, *args): calls.append("closed")
            def run(self, prompt, session_id):
                self_prompt = json.loads(prompt.split("\n", 1)[1])
                assert self_prompt["message"] == "/bash danger"
                assert not prompt.startswith("/")
                return SimpleNamespace(finish_reason="completed", final_response=json.dumps({"reply":"Safe answer", "proposal":None}))
        request = turn() | {"message": "/bash danger"}
        self.assertEqual(run_sdk(request, Path("/tmp/h"), Path("/tmp/w"), Fake)["backend"], "dsh")
        self.assertEqual(calls[0]["profile"], "sdk-minimal")
        self.assertTrue(calls[0]["patches"][0].endswith("no-tools.patch.yml"))
        self.assertEqual(calls[-1], "closed")

    def test_structured_proposal_is_draft_only(self):
        raw = json.dumps({"reply": "Please review", "proposal": {"owner": "bob", "summary": "review tests"}})
        self.assertEqual(parse_model_response(raw, "Ask bob to review tests")["proposal"]["owner"], "bob")

    def test_structured_response_rejects_invalid_or_unscoped_output(self):
        valid = {"reply": "Review first", "proposal": {"owner": "bob", "summary": "review tests"}}
        invalid = ["plain text", "```json\n{}\n```", '[]', '{"reply":"a","reply":"b","proposal":null}',
                   json.dumps(valid | {"execute": True}), json.dumps(valid | {"reply": " "}),
                   json.dumps(valid | {"proposal": {"owner": "bob", "summary": "secret from history"}}),
                   json.dumps(valid | {"proposal": {"owner": "eve", "summary": "review tests"}}),
                   json.dumps(valid | {"proposal": {"owner": "bob", "summary": "review tests", "confirmed": True}})]
        for raw in invalid:
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                parse_model_response(raw, "Ask bob to review tests")

    def test_sdk_incomplete_result_is_failure_and_closes(self):
        calls = []
        class Fake:
            def __init__(self, **config): pass
            def __enter__(self): return self
            def __exit__(self, *args): calls.append("closed")
            def run(self, *args, **kwargs):
                return SimpleNamespace(finish_reason="max_tokens", final_response="partial")
        with self.assertRaises(RuntimeError): run_sdk(turn(), Path("/tmp/h"), Path("/tmp/w"), Fake)
        self.assertEqual(calls, ["closed"])

    def test_worker_timeout_kills_group_and_removes_home(self):
        roots = []
        class Process:
            pid = 123456789
            def __init__(self, command, **kwargs):
                roots.append(Path(command[-1]))
                assert "SECRET_UNRELATED" not in kwargs["env"]
                assert "AGENCT_ADAPTER_TOKEN" not in kwargs["env"]
                assert kwargs["start_new_session"]
            def communicate(self, *args, **kwargs): raise subprocess.TimeoutExpired("worker", 1)
            def wait(self): pass
        with patch.dict("os.environ", {"DEEPSEEK_API_KEY": "fake-test-only", "SECRET_UNRELATED": "not-forwarded"}), \
             patch("adapter.subprocess.Popen", Process), patch("adapter.os.killpg") as kill:
            for _ in range(2):
                with self.assertRaises(subprocess.TimeoutExpired): DshRunner(1)(turn())
            self.assertEqual(kill.call_count, 2)
        self.assertNotEqual(roots[0], roots[1])
        self.assertTrue(all(not root.exists() for root in roots))

    def test_timeout_precedes_hub_deadline(self):
        self.assertEqual(DshRunner().timeout, 45)
        self.assertLess(DshRunner().timeout, 75)
        hub_source = Path(__file__).resolve().parents[2] / "hub/src/lib.rs"
        self.assertIn(".timeout(Duration::from_secs(75))", hub_source.read_text())

    def test_requires_strong_internal_token(self):
        with self.assertRaises(ValueError): make_server("short", lambda _: {}, 0)

    def test_http_auth_validation_and_runtime_error(self):
        calls = []
        def runner(data):
            calls.append(data)
            if data["message"] == "fail": raise RuntimeError("secret-provider-error")
            return {"backend": "dsh", "reply": "test fake, not a model"}
        server = make_server("t" * 32, runner, 0)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        def request(data, auth=True):
            client = http.client.HTTPConnection(*server.server_address)
            headers = {"Content-Type": "application/json"}
            if auth: headers["Authorization"] = "Bearer " + "t" * 32
            client.request("POST", "/v1/turn", json.dumps(data), headers)
            response = client.getresponse()
            result = response.status, json.loads(response.read())
            client.close()
            return result
        try:
            self.assertEqual(request(turn(), False)[0], 401)
            self.assertEqual(request(turn() | {"session_id": "bad"})[0], 400)
            self.assertEqual(calls, [])
            self.assertEqual(request(turn())[0], 200)
            self.assertEqual(request(turn() | {"message": "fail"}), (502, {"error": "runtime_failed"}))
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__": unittest.main()
