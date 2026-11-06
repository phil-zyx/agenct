import importlib.util
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("staging", Path(__file__).with_name("private-staging.py"))
staging = importlib.util.module_from_spec(spec)
spec.loader.exec_module(staging)


class PrivateStagingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.state = Path(self.temp.name) / "state"
        staging.initialize(self.state, ["alice", "bob"], 8080)

    def change(self, **changes):
        path = self.state / "config.json"
        config = json.loads(path.read_text())
        config.update(changes)
        path.write_text(json.dumps(config))

    def test_private_credentials_and_hashes(self):
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o700)
        for name in ("config.json", "credentials.json"):
            self.assertEqual((self.state / name).stat().st_mode & 0o777, 0o600)
        credentials = json.loads((self.state / "credentials.json").read_text())
        config = (self.state / "config.json").read_text()
        self.assertNotEqual(credentials["alice"], credentials["bob"])
        for token in credentials.values():
            self.assertNotIn(token, config)

    def test_overrides_unsafe_inherited_configuration(self):
        with patch.dict(os.environ, {"AGENCT_BIND": "0.0.0.0:8080", "AGENCT_DATABASE": "/tmp/shared.db", "AGENCT_RUNTIME": "dsh"}):
            env = staging.environment(self.state)
        self.assertEqual(env["AGENCT_BIND"], "127.0.0.1:8080")
        self.assertEqual(env["AGENCT_RUNTIME"], "mock")
        self.assertEqual(env["AGENCT_DATABASE"], str(self.state / "hub.sqlite3"))

    def test_does_not_overwrite(self):
        before = (self.state / "credentials.json").read_bytes()
        with self.assertRaises(FileExistsError):
            staging.initialize(self.state, ["alice"], 8080)
        self.assertEqual(before, (self.state / "credentials.json").read_bytes())

    def test_public_permissions_rejected(self):
        (self.state / "config.json").chmod(0o644)
        with self.assertRaises(ValueError):
            staging.environment(self.state)

    def test_directory_symlink_rejected(self):
        link = Path(self.temp.name) / "alias"
        link.symlink_to(self.state)
        with self.assertRaises(ValueError):
            staging.environment(link)

    def test_database_symlink_rejected(self):
        (self.state / "hub.sqlite3").symlink_to("missing")
        with self.assertRaises(ValueError):
            staging.environment(self.state)

    def test_schema_rejected(self):
        self.change(bind="0.0.0.0")
        with self.assertRaises(ValueError):
            staging.environment(self.state)

    def test_invalid_id_port_or_duplicate_rejected(self):
        for changes in ({"port": True}, {"port": 0}, {"users": []}, {"users": [{"id": "bad id", "token_sha256": "a" * 64}]}):
            with self.subTest(changes=changes):
                old = (self.state / "config.json").read_text()
                self.change(**changes)
                with self.assertRaises(ValueError):
                    staging.environment(self.state)
                (self.state / "config.json").write_text(old)

    def test_no_automatic_dsh_fallback(self):
        self.change(runtime="dsh")
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                staging.environment(self.state)

    def listen(self):
        sock = socket.socket()
        self.addCleanup(sock.close)
        sock.bind(("127.0.0.1", 0))
        sock.listen()
        return sock.getsockname()[1]

    def test_port_probe_detects_listener(self):
        port = self.listen()
        self.assertFalse(staging.port_available(port))
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            free = sock.getsockname()[1]
        self.assertTrue(staging.port_available(free))

    def test_run_refuses_busy_port_before_exec(self):
        port = self.listen()
        staging.set_port(self.state, port)
        env = staging.environment(self.state)
        with patch.object(staging, "BINARY", Path(__file__)), patch.object(staging, "stale_binary", lambda: False), \
                patch.object(staging.os, "execve", side_effect=AssertionError("must not exec")):
            with self.assertRaises(ValueError) as caught:
                staging.launch(self.state, env)
        self.assertIn("not the Hub for this state directory", str(caught.exception))
        self.assertIn("set-port", str(caught.exception))

    def test_running_hub_is_reported_instead_of_foreign_program(self):
        import fcntl
        lock = open(self.state / "hub.lock", "wb")
        self.addCleanup(lock.close)
        (self.state / "hub.lock").chmod(0o600)
        self.assertFalse(staging.hub_running(self.state))
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        self.assertTrue(staging.hub_running(self.state))
        env = staging.environment(self.state)
        with patch.object(staging, "BINARY", Path(__file__)), patch.object(staging, "stale_binary", lambda: False), \
                patch.object(staging.os, "execve", side_effect=AssertionError("must not exec")):
            with self.assertRaises(ValueError) as caught:
                staging.launch(self.state, env)
        self.assertIn("already running for this state directory", str(caught.exception))

    def test_set_port_preserves_credentials_and_permissions(self):
        before = (self.state / "credentials.json").read_bytes()
        users = json.loads((self.state / "config.json").read_text())["users"]
        staging.set_port(self.state, 18999)
        config = json.loads((self.state / "config.json").read_text())
        self.assertEqual(config["port"], 18999)
        self.assertEqual(config["users"], users)
        self.assertEqual(before, (self.state / "credentials.json").read_bytes())
        self.assertEqual((self.state / "config.json").stat().st_mode & 0o777, 0o600)
        self.assertFalse((self.state / "config.json.tmp").exists())
        self.assertEqual(staging.environment(self.state)["AGENCT_BIND"], "127.0.0.1:18999")
        for bad in (80, 70000, True):
            with self.assertRaises(ValueError):
                staging.set_port(self.state, bad)
        self.assertEqual(json.loads((self.state / "config.json").read_text())["port"], 18999)

    def test_set_port_rejects_unsafe_state(self):
        (self.state / "config.json").chmod(0o644)
        with self.assertRaises(ValueError):
            staging.set_port(self.state, 18999)

    def test_access_notice_never_contains_tokens(self):
        notice = staging.access_notice(self.state, staging.environment(self.state))
        self.assertIn("http://localhost:8080", notice)
        self.assertIn("alice, bob", notice)
        for token in json.loads((self.state / "credentials.json").read_text()).values():
            self.assertNotIn(token, notice)

    def test_start_initializes_on_free_port_when_default_busy(self):
        fresh = Path(self.temp.name) / "fresh"
        busy = self.listen()
        launched = []
        with patch.object(staging, "DEFAULT_PORT", busy), patch.object(staging, "build", lambda: None), \
                patch.object(staging, "launch", lambda directory, env: launched.append(env)):
            staging.start(fresh, ["alice", "bob"], None)
        port = json.loads((fresh / "config.json").read_text())["port"]
        self.assertNotEqual(port, busy)
        self.assertGreater(port, busy)  # Scans upward from the (patched) default.
        self.assertEqual(launched[0]["AGENCT_BIND"], f"127.0.0.1:{port}")
        self.assertEqual(launched[0]["AGENCT_RUNTIME"], "mock")

    def test_start_reuses_existing_state_without_rotating_tokens(self):
        before = (self.state / "credentials.json").read_bytes()
        with patch.object(staging, "build", lambda: None), patch.object(staging, "launch", lambda directory, env: None):
            staging.start(self.state, ["alice", "bob"], None)
            with self.assertRaises(ValueError):
                staging.start(self.state, ["alice", "bob"], 9000)
        self.assertEqual(before, (self.state / "credentials.json").read_bytes())

    def test_start_rejects_explicit_busy_port_without_creating_state(self):
        fresh = Path(self.temp.name) / "fresh"
        with self.assertRaises(ValueError):
            staging.start(fresh, ["alice"], self.listen())
        self.assertFalse(fresh.exists())


if __name__ == "__main__":
    unittest.main()
