#!/usr/bin/env python3
"""Private, loopback-only staging. Never prints credentials or provisions services."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import stat
import sys

ROOT = Path(__file__).resolve().parents[1]


def secure_path(path, directory=False):
    info = path.lstat()
    expected = stat.S_ISDIR if directory else stat.S_ISREG
    if not expected(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise ValueError(f"{path}: must be owner-only, owned by this user, and not a symlink")


def write_private(path, value):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(value, stream, indent=2)
        stream.write("\n")


def initialize(directory, users, port):
    if not users or len(set(users)) != len(users) or any(not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", user) for user in users):
        raise ValueError("supply distinct user IDs of 1–64 ASCII letters, digits, underscores or hyphens")
    if not 1024 <= port <= 65535:
        raise ValueError("port must be between 1024 and 65535")
    # Fail closed on existing directories: never rotate credentials or overwrite data implicitly.
    directory.mkdir(mode=0o700)
    credentials = {user: secrets.token_urlsafe(32) for user in users}
    config = {
        "version": 1,
        "port": port,
        "runtime": "mock",
        "users": [{"id": user, "token_sha256": hashlib.sha256(token.encode()).hexdigest()} for user, token in credentials.items()],
    }
    write_private(directory / "config.json", config)
    write_private(directory / "credentials.json", credentials)


def environment(directory):
    secure_path(directory, directory=True)
    config_path = directory / "config.json"
    secure_path(config_path)
    config = json.loads(config_path.read_text())
    if not isinstance(config, dict) or set(config) != {"version", "port", "runtime", "users"} or type(config["version"]) is not int or config["version"] != 1:
        raise ValueError("unsupported configuration schema")
    port = config["port"]
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError("port must be between 1024 and 65535")
    if config["runtime"] not in ("mock", "dsh"):
        raise ValueError("runtime must explicitly be mock or dsh")
    users = config["users"]
    if not isinstance(users, list) or not users:
        raise ValueError("at least one user required")
    ids, hashes = set(), set()
    for user in users:
        if not isinstance(user, dict) or set(user) != {"id", "token_sha256"}:
            raise ValueError("invalid user configuration")
        uid, digest = user["id"], user["token_sha256"]
        if not isinstance(uid, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", uid) or not isinstance(digest, str) or not re.fullmatch(r"[0-9a-f]{64}", digest) or uid in ids or digest in hashes:
            raise ValueError("invalid or duplicate identity")
        ids.add(uid)
        hashes.add(digest)
    for name in ("hub.sqlite3", "hub.sqlite3-wal", "hub.sqlite3-shm", "hub.lock"):
        path = directory / name
        if path.exists() or path.is_symlink():
            secure_path(path)
    env = os.environ.copy()
    env.update(AGENCT_BIND=f"127.0.0.1:{port}", AGENCT_DATABASE=str(directory / "hub.sqlite3"),
               AGENCT_RUNTIME=config["runtime"], AGENCT_USERS_JSON=json.dumps(users))
    if config["runtime"] == "dsh" and not all(env.get(key) for key in ("AGENCT_ADAPTER_URL", "AGENCT_ADAPTER_TOKEN")):
        raise ValueError("dsh requires an independently configured local adapter URL and token; see docs/dsh-adapter.md")
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("init", "check", "run"))
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--users", nargs="+", default=["alice", "bob"])
    parser.add_argument("--port", type=int, default=8080)
    args = parser.parse_args()
    # Do not resolve final component: lstat must detect directory symlinks.
    directory = Path(os.path.abspath(args.state_dir))
    if args.action == "init":
        initialize(directory, args.users, args.port)
        print("Private staging initialized. Credentials are in the owner-only credentials.json file; keep them private.")
        return
    env = environment(directory)
    if args.action == "check":
        print(f"Configuration valid: loopback-only, runtime={env['AGENCT_RUNTIME']}. This is not a live service or provider check.")
        return
    binary = ROOT / "hub/target/release/agenct-hub"
    if not binary.is_file():
        raise ValueError("build first: cargo build --locked --release --manifest-path hub/Cargo.toml")
    os.umask(0o077)
    os.chdir(ROOT)
    os.execve(binary, [str(binary)], env)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, json.JSONDecodeError) as exc:
        print(f"Private staging refused: {exc}", file=sys.stderr)
        sys.exit(1)
