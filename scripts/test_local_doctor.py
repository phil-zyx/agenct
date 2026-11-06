"""Offline, injected-platform checks. No sockets, tool execution or real credentials."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("doctor", Path(__file__).with_name("local-doctor.py"))
doctor = importlib.util.module_from_spec(spec)
spec.loader.exec_module(doctor)

LINUX_MOUNT = "10 1 8:1 / / rw - ext4 /dev/sda rw\n"


class LocalDoctorTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="doctor test ")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "repo with spaces"
        self.state = Path(self.temp.name) / "private state"
        for name in ("hub/web/index.html", "hub/web/client.js", "scripts/private-staging.py", "hub/target/release/agenct-hub"):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture only")
        self.binary = self.root / "hub/target/release/agenct-hub"
        self.binary.chmod(0o700)
        for target in ("socket.socket", "subprocess.Popen", "os.execve", "os.system"):
            guard = patch(target, side_effect=AssertionError("doctor attempted runtime/network execution"))
            guard.start()
            self.addCleanup(guard.stop)

    def inspect(self, **changes):
        args = dict(system="Linux", release="6.6.0-microsoft-standard-WSL2", env={},
                    python_version=(3, 12), which=lambda name: "/not-executed/" + name,
                    mountinfo=LINUX_MOUNT)
        args.update(changes)
        return doctor.inspect_local(self.root, self.state, **args)

    def test_wsl_versions_and_non_wsl_are_distinct(self):
        cases = [
            ("Linux", "6.6.87.2-microsoft-standard-WSL2", {}, "wsl2"),
            ("Linux", "4.4.0-19041-Microsoft", {}, "wsl1"),
            ("Linux", "5.15-custom-microsoft", {}, "wsl-unknown"),
            ("Linux", "6.8.0-custom", {"WSL_DISTRO_NAME": "fixture"}, "wsl-unknown"),
            ("Linux", "6.8.0-custom", {"WSL_INTEROP": "fixture"}, "wsl-unknown"),
            ("Linux", "6.8.0-generic", {}, "linux"),
            ("Darwin", "24.0", {}, "non-linux"),
            ("Windows", "10", {"WSL_DISTRO_NAME": "fixture"}, "non-linux"),
        ]
        for system, release, env, expected in cases:
            with self.subTest(expected=expected, release=release):
                self.assertEqual(doctor.host_kind(system, release, env), expected)

    def test_ordinary_linux_does_not_claim_wsl(self):
        results = self.inspect(release="6.8.0-generic")
        self.assertIn(("NOTE", "Ordinary Linux detected; this is not evidence of a WSL test."), results)
        self.assertNotIn("FAIL", [level for level, _ in results])

    def test_non_linux_and_wsl1_block_quickstart(self):
        for changes in ({"system": "Windows"}, {"system": "Darwin"}, {"release": "4.4.0-19041-Microsoft"}):
            with self.subTest(changes=changes):
                self.assertEqual(self.inspect(**changes)[0][0], "FAIL")

    def test_unknown_wsl_requires_operator_confirmation(self):
        self.assertEqual(self.inspect(release="custom", env={"WSL_INTEROP": "fixture"})[0][0], "WARN")

    def test_python_requirements(self):
        old = self.inspect(python_version=(3, 8))
        self.assertTrue(any(level == "FAIL" and "Python 3.9+" in message for level, message in old))
        supported = self.inspect(python_version=(3, 9))
        self.assertFalse(any(level == "FAIL" for level, _ in supported))
        self.assertTrue(any("separate Python 3.11+" in message for _, message in supported))

    def test_missing_tools_and_build(self):
        self.binary.unlink()
        results = self.inspect(which=lambda name: None)
        for name in ("cargo", "rustc"):
            self.assertTrue(any(level == "FAIL" and message.startswith(name + " is missing") for level, message in results))
        for name in ("git", "node", "npm"):
            self.assertTrue(any(level == "WARN" and message.startswith(name + " is missing") for level, message in results))
        self.assertTrue(any(level == "FAIL" and message.startswith("Release Hub") for level, message in results))

    def test_prebuilt_does_not_require_cargo_to_be_present(self):
        results = self.inspect(which=lambda name: None)
        self.assertFalse(any(level == "FAIL" for level, _ in results))
        self.assertTrue(any("revision/compatibility were not verified" in message for _, message in results))

    def test_build_requires_executable_and_assets(self):
        self.binary.chmod(0o600)
        (self.root / "hub/web/index.html").unlink()
        results = self.inspect()
        self.assertEqual(sum(level == "FAIL" for level, _ in results), 2)

    def test_windows_mount_fallback_and_custom_drvfs(self):
        for path in ("/mnt/c/repo", "/mnt/D/private state"):
            with self.subTest(path=path):
                self.assertEqual(doctor.filesystem_kind(Path(path), None), "windows")
        for fs, options in (("drvfs", "rw"), ("9p", "rw,aname=drvfs;path=C:\\;uid=1000"), ("ntfs3", "rw")):
            mountinfo = LINUX_MOUNT + f"11 10 0:42 / /windows rw - {fs} C:\\ {options}\n"
            with self.subTest(fs=fs):
                self.assertEqual(doctor.filesystem_kind(Path("/windows/private"), mountinfo), "windows")

    def test_mount_escapes_and_longest_mount_match(self):
        mounts = (LINUX_MOUNT + "11 10 0:42 / /shared\\040drive rw - 9p C:\\ rw,aname=drvfs\n"
                  "12 11 8:2 / /shared\\040drive/native rw - ext4 /dev/sdb rw\n")
        self.assertEqual(doctor.filesystem_kind(Path("/shared drive/state"), mounts), "windows")
        self.assertEqual(doctor.filesystem_kind(Path("/shared drive/native/state"), mounts), "other")

    def test_windows_mount_symlink_alias(self):
        alias = Path(self.temp.name) / "alias"
        alias.symlink_to("/mnt/c/fixture-does-not-need-to-exist")
        self.assertEqual(doctor.filesystem_kind(alias / "state", LINUX_MOUNT), "windows")

    def test_windows_state_blocks_but_checkout_warns(self):
        mount = str(Path(self.temp.name)).replace(" ", "\\040")
        results = self.inspect(mountinfo=LINUX_MOUNT + f"11 10 0:42 / {mount} rw - drvfs C:\\ rw\n")
        self.assertTrue(any(level == "WARN" and message.startswith("Checkout appears") for level, message in results))
        self.assertTrue(any(level == "FAIL" and message.startswith("Private state appears") for level, message in results))

    def test_unknown_and_malformed_mount_metadata_are_not_certified(self):
        for mountinfo in (None, "malformed", "11 10 0:42 / / rw - 9p other rw\n"):
            with self.subTest(mountinfo=mountinfo):
                self.assertEqual(doctor.filesystem_kind(self.state, mountinfo), "unknown")
                self.assertTrue(any(level == "WARN" and "could not be classified" in message for level, message in self.inspect(mountinfo=mountinfo)))

    def test_state_must_stay_outside_checkout(self):
        self.state = self.root / "private"
        self.assertTrue(any(level == "FAIL" and "outside the checkout" in message for level, message in self.inspect()))

    def test_state_parent_alias_cannot_hide_checkout_location(self):
        alias = Path(self.temp.name) / "checkout alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.state = alias / "private"
        self.assertTrue(any(level == "FAIL" and "outside the checkout" in message for level, message in self.inspect()))

    def test_state_symlink_and_missing_parent(self):
        self.state.symlink_to("missing")
        self.assertTrue(any(level == "FAIL" and "must not be a symlink" in message for level, message in self.inspect()))
        self.state.unlink()
        self.state = self.state / "child"
        self.assertTrue(any(level == "FAIL" and "parent directory is missing" in message for level, message in self.inspect()))

    def test_read_only_no_secret_or_environment_output(self):
        self.state.mkdir(mode=0o700)
        secret = "SENTINEL-DO-NOT-READ-OR-PRINT"
        (self.state / "credentials.json").write_text(secret)
        env = {"DEEPSEEK_API_KEY": secret, "AGENCT_ADAPTER_TOKEN": secret,
               "AGENCT_OIDC_CONFIG_FILE": secret, "AGENCT_BROWSER_OIDC_CONFIG_FILE": secret,
               "AGENCT_WORKER_SCOPES_FILE": secret}
        with patch("builtins.open", side_effect=AssertionError("doctor read file contents")), \
                patch.object(Path, "read_text", side_effect=AssertionError("doctor read file contents")), \
                patch.object(Path, "mkdir", side_effect=AssertionError("doctor wrote a directory")), \
                patch("os.chmod", side_effect=AssertionError("doctor changed permissions")):
            results = self.inspect(env=env)
        output = str(results)
        self.assertNotIn(secret, output)
        self.assertIn("Inherited optional OIDC/browser/worker", output)
        self.assertIn("Contents were not read", output)
        self.assertIn(doctor.LIMITS, output)

    def test_missing_state_is_not_created(self):
        self.inspect()
        self.assertFalse(self.state.exists())

    def test_cli_exit_status_and_no_path_leak_on_inspection_error(self):
        for level, expected in (("NOTE", 0), ("WARN", 0), ("FAIL", 1)):
            output = io.StringIO()
            with patch.object(doctor, "inspect_local", return_value=[(level, "fixture")]), \
                    contextlib.redirect_stdout(output):
                self.assertEqual(doctor.main(["--state-dir", str(self.state)]), expected)
            self.assertIn("not runtime readiness", output.getvalue())
        output = io.StringIO()
        with patch.object(doctor, "inspect_local", side_effect=OSError("private-path-sentinel")), \
                contextlib.redirect_stdout(output):
            self.assertEqual(doctor.main([]), 1)
        self.assertNotIn("private-path-sentinel", output.getvalue())


if __name__ == "__main__":
    unittest.main()
