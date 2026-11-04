import importlib.util
import json
import os
from pathlib import Path
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


if __name__ == "__main__":
    unittest.main()
