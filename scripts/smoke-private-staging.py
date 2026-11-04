#!/usr/bin/env python3
"""Exercise the real staging launcher on loopback; no model calls or remote writes."""
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
LAUNCHER = ROOT / "scripts/private-staging.py"


def main():
    if not (ROOT / "hub/target/release/agenct-hub").is_file():
        raise RuntimeError("build the release Hub first")
    with tempfile.TemporaryDirectory(prefix="agenct-private-smoke-") as temp:
        state = Path(temp) / "state"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        base = f"http://127.0.0.1:{port}"
        subprocess.run([sys.executable, str(LAUNCHER), "init", "--state-dir", str(state), "--port", str(port)], check=True, capture_output=True)
        tokens = json.loads((state / "credentials.json").read_text())

        def request(path, token=None, body=None):
            headers = {"Content-Type": "application/json"}
            if token:
                headers["Authorization"] = "Bearer " + token
            req = urllib.request.Request(base + path, headers=headers, data=json.dumps(body).encode() if body is not None else None)
            try:
                with urllib.request.urlopen(req, timeout=3) as response:
                    return response.status, json.load(response)
            except urllib.error.HTTPError as exc:
                return exc.code, json.load(exc)

        for restart in (False, True):
            # Captured logs stay private; do not leak credentials or raw configuration.
            process = subprocess.Popen([sys.executable, str(LAUNCHER), "run", "--state-dir", str(state)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            try:
                deadline = time.monotonic() + 10
                while True:
                    if process.poll() is not None:
                        raise RuntimeError("Hub exited during startup")
                    try:
                        status, _ = request("/api/me")
                        if status != 401:
                            raise AssertionError("unauthenticated request was not rejected")
                        break
                    except (urllib.error.URLError, ConnectionError):
                        if time.monotonic() >= deadline:
                            raise RuntimeError("Hub startup timeout")
                        time.sleep(0.05)
                with urllib.request.urlopen(base + "/", timeout=3) as response:
                    page = response.read().decode()
                    assert response.status == 200 and "<html" in page.lower()
                    assert all(token not in page for token in tokens.values())
                assert request("/api/me", "invalid")[0] == 401
                status, me = request("/api/me", tokens["alice"])
                assert status == 200 and me["user"] == "alice"
                if not restart:
                    assert request("/api/messages", tokens["alice"], {"message": "private staging persistence marker"})[0] == 200
                status, messages = request("/api/messages", tokens["alice"])
                assert status == 200 and "private staging persistence marker" in json.dumps(messages)
                assert "private staging persistence marker" not in json.dumps(request("/api/messages", tokens["bob"])[1])
                for path in state.iterdir():
                    assert path.stat().st_mode & 0o077 == 0, "state file permissions too broad"
            finally:
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
        print("PASS: real launcher startup/restart, unauthorized rejection, identity, private chat persistence and owner-only files (mock runtime).")


if __name__ == "__main__":
    main()
