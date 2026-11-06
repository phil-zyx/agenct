#!/usr/bin/env python3
"""Real Hub -> real adapter HTTP smoke, with a TEST-ONLY deterministic runner.

Requires a built release Hub and permission to open loopback sockets. No SDK is
loaded, no provider is contacted, and no production fake-runtime switch is added.
The restart checks cover Hub SQLite receipts/history, not DSH session durability.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import secrets
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = Path(__file__).resolve()
LAUNCHER = ROOT / "scripts/private-staging.py"
ALICE_PRIVATE = "fixture alice private: café notes, never shared"
BOB_PRIVATE = "fixture bob private: separate notes, never shared"
PROPOSAL_MESSAGE = "bob: review fixture-only notes"
PROPOSAL_SUMMARY = "review fixture-only notes"
FAILURE_MESSAGE = "fixture injected runner failure"
MALFORMED_MESSAGE = "fixture injected malformed adapter reply"


def load_module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


adapter = load_module("smoke_adapter", ROOT / "adapters/dsh/adapter.py")
staging = load_module("smoke_staging", LAUNCHER)


def check(condition, description):
    # Keep acceptance checks active even under python -O. Never include secrets.
    if not condition:
        raise AssertionError(description)


def isolated_environment(home):
    """Do not inherit provider keys, proxies, Python hooks, or Hub configuration."""
    return {
        "PATH": os.defpath,
        "LANG": "C.UTF-8",
        "HOME": str(home),
        "TMPDIR": str(home),
    }


class FixtureRunner:
    """Deliberate test double; only this smoke script can construct it."""

    def __init__(self, observations):
        self.observations = observations
        self.lock = threading.Lock()

    def __call__(self, turn):
        # make_server already validated the real HTTP payload. Record before any
        # injected failure so a second dispatch is visible, including on restart.
        with self.lock:
            fd = os.open(self.observations, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)
            with os.fdopen(fd, "a") as stream:
                stream.write(json.dumps(turn, ensure_ascii=False) + "\n")
                stream.flush()
                os.fsync(stream.fileno())
        if turn["message"] == FAILURE_MESSAGE:
            raise RuntimeError("test-only failure after dispatch")
        if turn["message"] == MALFORMED_MESSAGE:
            # Intentional wire fault: bypass the parser ONLY for this negative
            # fixture, so the real Hub must reject a missing backend/proposal.
            return {"reply": "[TEST FIXTURE ONLY] malformed output"}
        proposal = None
        if turn["message"] == PROPOSAL_MESSAGE:
            proposal = {"owner": "bob", "summary": PROPOSAL_SUMMARY}
        # Successful fixtures pass the production response parser. This does not
        # exercise run_sdk, prompt construction, a model, tools, or DshRunner.
        return adapter.parse_model_response(json.dumps({
            "reply": "[TEST FIXTURE ONLY, no model] " + turn["message"],
            "proposal": proposal,
        }), turn["message"])


def serve_fixture(directory):
    # The driver starts this separate process with an explicit empty-of-secrets
    # environment and private home. It never constructs a production DshRunner.
    check(not os.environ.get("DEEPSEEK_API_KEY"), "fixture must not have a provider key")
    token = json.loads((directory / "fixture.json").read_text())["token"]
    server = adapter.make_server(token, FixtureRunner(directory / "calls.jsonl"), 0)
    try:
        staging.write_private(directory / "ready.tmp", {"port": server.server_port})
        os.replace(directory / "ready.tmp", directory / "ready.json")
        try:
            server.serve_forever(poll_interval=0.05)
        except KeyboardInterrupt:
            pass
    finally:
        server.server_close()


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def request(base, path, token=None, body=None, method=None):
    parsed = urllib.parse.urlsplit(base)
    check(parsed.scheme == "http" and parsed.hostname == "127.0.0.1"
          and not parsed.username and not parsed.password and not parsed.path
          and not parsed.query and not parsed.fragment, "smoke HTTP must stay on loopback")
    check(path.startswith("/") and not path.startswith("//"), "invalid smoke request path")
    headers = {"Content-Type": "application/json"}
    if token is not None:
        headers["Authorization"] = "Bearer " + token
    req = urllib.request.Request(
        base + path, headers=headers, method=method,
        data=json.dumps(body, ensure_ascii=False).encode() if body is not None else None,
    )
    # Never use inherited proxies or follow redirects; no mutation is retried.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(req, timeout=5) as response:
            return response.status, json.load(response)
    except urllib.error.HTTPError as exc:
        with exc:
            return exc.code, json.load(exc)


def wait_ready(process, ready, label):
    deadline = time.monotonic() + 15
    while True:
        if process.poll() is not None:
            raise RuntimeError(f"{label} exited during startup")
        if ready():
            return
        if time.monotonic() >= deadline:
            raise RuntimeError(f"{label} startup timeout")
        time.sleep(0.05)


def stop(process):
    if process is None:
        return
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)
            raise RuntimeError("smoke child did not stop gracefully")
    else:
        process.wait()
    check(process.returncode == 0, "smoke child exited unsuccessfully")


def free_port():
    # The Hub does not accept an inherited socket. A bind race fails startup,
    # rather than switching to a remote endpoint or attaching to an old process.
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def route(revision=0, agent_id="butler", role="coordinator"):
    return {"revision": revision, "agent_id": agent_id, "role": role, "model_ref": "server-default"}


def profile(revision, research_role="researcher"):
    return {"expected_revision": revision, "agents": [
        {"id": "butler", "name": "Fixture coordinator", "role": "coordinator",
         "model_ref": "server-default", "enabled": True},
        {"id": "research", "name": "Fixture personal role", "role": research_role,
         "model_ref": "server-default", "enabled": True},
    ]}


class BridgeSmoke:
    def __init__(self, directory):
        self.directory = directory
        self.home = directory / "home"
        self.home.mkdir(mode=0o700)
        self.state = directory / "state"
        self.adapter_process = None
        self.hub_process = None
        self.adapter_token = secrets.token_urlsafe(32)
        staging.write_private(directory / "fixture.json", {"token": self.adapter_token})
        staging.initialize(self.state, ["alice", "bob"], free_port())
        config = json.loads((self.state / "config.json").read_text())
        config["runtime"] = "dsh"
        (self.state / "config.json").write_text(json.dumps(config))
        self.base = f"http://127.0.0.1:{config['port']}"
        self.tokens = json.loads((self.state / "credentials.json").read_text())
        self.histories = {"alice": [], "bob": []}
        self.sessions = {}

    def calls(self):
        path = self.directory / "calls.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def start(self):
        ready = self.directory / "ready.json"
        ready.unlink(missing_ok=True)
        self.adapter_process = subprocess.Popen(
            [sys.executable, "-I", str(SCRIPT), "--fixture-adapter", str(self.directory)],
            env=isolated_environment(self.home), cwd=ROOT,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        wait_ready(self.adapter_process, ready.exists, "adapter fixture")
        self.adapter_base = f"http://127.0.0.1:{json.loads(ready.read_text())['port']}"
        env = isolated_environment(self.home) | {
            "AGENCT_ADAPTER_URL": self.adapter_base + "/v1/turn",
            "AGENCT_ADAPTER_TOKEN": self.adapter_token,
        }
        self.hub_process = subprocess.Popen(
            [sys.executable, "-I", str(LAUNCHER), "run", "--state-dir", str(self.state)],
            env=env, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )

        def hub_ready():
            try:
                return request(self.base, "/api/me")[0] == 401
            except (urllib.error.URLError, ConnectionError, TimeoutError):
                return False

        wait_ready(self.hub_process, hub_ready, "Hub")
        for owner in self.tokens:
            me = self.api(owner, "/api/me")
            check(me["user"] == owner and me["runtime"] == "dsh", "wrong identity or runtime")

    def stop(self):
        # Always reap both processes, including when one cleanup fails.
        try:
            stop(self.hub_process)
        finally:
            self.hub_process = None
            try:
                stop(self.adapter_process)
            finally:
                self.adapter_process = None

    def api(self, owner, path, body=None, *, method=None, status=200):
        code, result = request(self.base, path, self.tokens[owner], body, method)
        check(code == status, f"unexpected HTTP status at {path}: {code}, expected {status}")
        return result

    def unchanged(self, count):
        check(len(self.calls()) == count, "unexpected extra runner dispatch")

    def dispatch(self, owner, payload, expected_route, *, failure=False):
        before = len(self.calls())
        result = self.api(owner, "/api/messages", payload, status=502 if failure else 200)
        calls = self.calls()
        check(len(calls) == before + 1, "turn must dispatch exactly once")
        turn = calls[-1]
        check(set(turn) == {"session_id", "history", "message", "runtime_profile"},
              "Hub forwarded unexpected fields to adapter")
        check(turn["message"] == payload["message"], "wrong message crossed HTTP boundary")
        check(turn["history"] == self.histories[owner], "history is not completed-only owner history")
        check(turn["runtime_profile"] == expected_route, "wrong selected role or revision at runner")
        session = turn["session_id"]
        check(str(uuid.UUID(session)) == session, "adapter session is not a canonical UUID")
        if owner in self.sessions:
            check(session == self.sessions[owner], "owner session changed across turns or restart")
        else:
            check(session not in self.sessions.values(), "owners share an adapter session")
            self.sessions[owner] = session
        encoded = json.dumps(turn)
        check(all(token not in encoded for token in [self.adapter_token, *self.tokens.values()]),
              "a credential entered the adapter payload")
        if not failure:
            check(result["backend"] == "dsh" and result["runtime_profile"] == expected_route,
                  "Hub changed the adapter response route or fell back to mock")
            check(result["reply"] == "[TEST FIXTURE ONLY, no model] " + payload["message"],
                  "Hub did not return the deterministic adapter reply")
            check(result["turn"] == {"request_id": payload["request_id"], "status": "complete"},
                  "successful turn lacks a completed receipt")
            self.histories[owner] += [{"role": "user", "content": payload["message"]},
                                      {"role": "assistant", "content": result["reply"]}]
        return result

    def rejected_selection(self, owner, payload, status, code):
        before = len(self.calls())
        result = self.api(owner, "/api/messages", payload, status=status)
        check(result.get("code") == code, "selection failure lost its pre-dispatch error code")
        self.api(owner, "/api/turns/" + payload["request_id"], status=404)
        self.unchanged(before)

    def uncertain(self, payload):
        key = payload["request_id"]
        before = len(self.calls())
        receipt = self.api("alice", "/api/turns/" + key)
        check(receipt == {"request_id": key, "status": "uncertain", "response": None},
              "failed turn does not retain an uncertain receipt")
        replay = self.api("alice", "/api/messages", payload)
        check(replay == {"reply": None, "turn": {"request_id": key, "status": "uncertain"}},
              "uncertain turn was replayed as a successful answer")
        self.api("bob", "/api/turns/" + key, status=404)
        self.unchanged(before)

    def check_messages(self, failures):
        for owner in self.tokens:
            messages = self.api(owner, "/api/messages")
            completed = [{"role": m["role"], "content": m["content"]}
                         for m in messages if m["status"] == "complete"]
            check(completed == self.histories[owner], "private completed message history changed")
            uncertain = [m for m in messages if m["status"] != "complete"]
            expected = [{"role": "user", "content": p["message"], "status": "uncertain"}
                        for p in failures] if owner == "alice" else []
            check(uncertain == expected, "failed turn message status or owner isolation changed")

    def exercise(self):
        self.start()
        # These requests hit the real Python handler. Browser/Hub user tokens
        # cannot substitute for the independently generated internal bearer.
        valid_turn = {"session_id": str(uuid.uuid4()), "history": [], "message": "auth probe"}
        for token in (None, "invalid", *self.tokens.values()):
            check(request(self.adapter_base, "/v1/turn", token, valid_turn)[0] == 401,
                  "adapter accepted missing, invalid, or end-user authentication")
        check(request(self.adapter_base, "/v1/turn", self.adapter_token,
                      valid_turn | {"session_id": "invalid"})[0] == 400,
              "adapter accepted an invalid Hub payload")
        check(request(self.base, "/api/me", self.adapter_token)[0] == 401,
              "internal adapter token authenticated to Hub")
        self.unchanged(0)

        first = {"request_id": "same-owner-private-key", "message": ALICE_PRIVATE}
        alice_response = self.dispatch("alice", first, route())
        self.dispatch("bob", first | {"message": BOB_PRIVATE}, route())
        self.api("alice", "/api/profile", profile(0), method="PUT")
        check(self.api("bob", "/api/profile")["revision"] == 0, "profile leaked across owners")
        selected = {"request_id": "selected-role", "message": "fixture alice selected research",
                    "agent_id": "research", "expected_profile_revision": 1}
        selected_response = self.dispatch("alice", selected, route(1, "research", "researcher"))
        self.dispatch("bob", {"request_id": "bob-second", "message": "fixture bob second turn"}, route())
        before = len(self.calls())
        self.rejected_selection("bob", selected | {"expected_profile_revision": 0}, 400,
                                "selected_agent_unavailable_before_dispatch")
        self.rejected_selection("alice", selected | {"request_id": "stale-first", "expected_profile_revision": 0},
                                409, "selected_agent_stale_before_dispatch")
        self.api("alice", "/api/profile", profile(1, "reviewer"), method="PUT")
        check(self.api("alice", "/api/messages", selected) == selected_response,
              "completed receipt changed after profile edit")
        self.rejected_selection("alice", selected | {"request_id": "stale-after-edit"},
                                409, "selected_agent_stale_before_dispatch")
        self.unchanged(before)
        self.dispatch("alice", selected | {"request_id": "new-role", "expected_profile_revision": 2},
                      route(2, "research", "reviewer"))

        proposed = self.dispatch("alice", {"request_id": "draft", "message": PROPOSAL_MESSAGE}, route(2))
        proposal = proposed["proposal"]
        check(proposal["requester"] == "alice" and proposal["owner"] == "bob"
              and proposal["summary"] == PROPOSAL_SUMMARY and proposal["status"] == "pending"
              and proposal["revision"] == 0 and proposal["task_id"] is None,
              "proposal did not remain an immutable private draft")
        check(self.api("alice", "/api/proposals") == [proposal], "requester cannot see private draft")
        check(self.api("bob", "/api/proposals") == [], "recipient saw unconfirmed private draft")
        for owner in self.tokens:
            check(self.api(owner, "/api/tasks") == [], "draft created a task before confirmation")
        action_path = "/api/proposals/" + proposal["id"] + "/actions"
        action = {"action": "confirm", "expected_revision": 0}
        self.api("bob", action_path, action, status=404)
        self.api("alice", action_path, action | {"expected_revision": 1}, status=409)
        check(self.api("bob", "/api/tasks") == [], "unauthorized or stale confirmation created a task")
        before = len(self.calls())
        confirmed = self.api("alice", action_path, action)
        check(confirmed["status"] == "confirmed" and confirmed["revision"] == 1,
              "explicit confirmation did not confirm the proposal")
        check(self.api("alice", action_path, action) == confirmed, "confirmation retry changed the task")
        tasks = self.api("bob", "/api/tasks")
        check(len(tasks) == 1 and tasks[0] == {
            "id": confirmed["task_id"], "requester": "alice", "owner": "bob",
            "summary": PROPOSAL_SUMMARY, "status": "awaiting_owner", "revision": 0, "result": None,
        }, "recipient saw more than the confirmed shared task")
        self.unchanged(before)

        failures = [{"request_id": "failure", "message": FAILURE_MESSAGE},
                    {"request_id": "malformed", "message": MALFORMED_MESSAGE}]
        for payload in failures:
            self.dispatch("alice", payload, route(2), failure=True)
            self.uncertain(payload)
        check(self.api("bob", "/api/tasks") == tasks, "failed output created a task")
        check(self.api("alice", "/api/proposals") == [confirmed], "failed output created a proposal")
        self.check_messages(failures)
        before = len(self.calls())
        self.stop()
        self.start()  # Same SQLite/auth; new adapter process, no persisted SDK state.
        check(self.api("alice", "/api/messages", first) == alice_response, "restart lost completed receipt")
        check(self.api("alice", "/api/messages", selected) == selected_response,
              "restart rerouted a completed selected-role receipt")
        for payload in failures:
            self.uncertain(payload)
        check(self.api("alice", "/api/profile")["revision"] == 2, "restart lost profile revision")
        check(self.api("bob", "/api/tasks") == tasks, "restart changed confirmed task")
        check(self.api("alice", "/api/proposals") == [confirmed], "restart changed proposal")
        self.check_messages(failures)
        self.unchanged(before)
        # Fresh post-restart turns prove session continuity and that neither
        # uncertain input nor another owner's private text enters the history.
        for owner, expected_route in (("alice", route(2)), ("bob", route())):
            self.dispatch(owner, {"request_id": "after-restart", "message": f"fixture {owner} after restart"},
                          expected_route)
        self.check_messages(failures)
        for path in self.state.iterdir():
            check(path.stat().st_mode & 0o077 == 0, "Hub state permissions are too broad")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture-adapter", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.fixture_adapter is not None:
        serve_fixture(args.fixture_adapter)
        return
    if not (ROOT / "hub/target/release/agenct-hub").is_file():
        raise RuntimeError("build the release Hub first")
    with tempfile.TemporaryDirectory(prefix="agenct-hub-adapter-smoke-") as temp:
        smoke = BridgeSmoke(Path(temp))
        try:
            smoke.exercise()
        finally:
            smoke.stop()
    print("PASS: real Hub and adapter HTTP boundary; isolated owner histories/sessions, selected role revisions, "
          "private proposal confirmation, uncertain no-redispatch receipts, and Hub restart persistence. "
          "TEST FIXTURE ONLY: no SDK, model, tools, or DSH persistence tested.")


if __name__ == "__main__":
    main()
