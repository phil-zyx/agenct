#!/usr/bin/env python3
"""Private, loopback-only staging. Never prints credentials or provisions services."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import socket
import stat
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "hub/target/release/agenct-hub"
BUILD = ["cargo", "build", "--locked", "--release", "--manifest-path", str(ROOT / "hub/Cargo.toml")]
DEFAULT_PORT = 8080


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
    valid_port(port)
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


def valid_port(port):
    if type(port) is not int or not 1024 <= port <= 65535:
        raise ValueError("port must be between 1024 and 65535")


def port_available(port):
    """Mirror the Hub's loopback bind. Advisory only: another program can still race us."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        # Tokio sets SO_REUSEADDR on Unix, so TIME_WAIT must not count as busy here either.
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            sock.bind(("127.0.0.1", port))
        except OSError:
            return False
    return True


def busy_port_message(port, directory):
    return (f"127.0.0.1:{port} is already in use by another program. Whatever answers at "
            f"http://localhost:{port} is not the Hub for this state directory. Stop that program, "
            f"or pick a free port with: "
            f"python3 scripts/private-staging.py set-port --state-dir {directory} --port <PORT>")


def hub_running(directory):
    """True if a Hub holds this state directory's exclusive lock (the Hub uses flock via fs2)."""
    path = directory / "hub.lock"
    if not path.is_file() or path.is_symlink():
        return False
    with open(path, "rb") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(lock, fcntl.LOCK_UN)
    return False


def first_free_port(attempts=20):
    start = DEFAULT_PORT
    for port in range(start, min(start + attempts, 65536)):
        if port_available(port):
            return port
    raise ValueError(f"no free loopback port in {start}-{start + attempts - 1}; pass --port explicitly")


def set_port(directory, port):
    valid_port(port)
    environment(directory)  # Validate ownership, permissions and schema before rewriting.
    path = directory / "config.json"
    config = json.loads(path.read_text())
    config["port"] = port
    temporary = directory / "config.json.tmp"
    if temporary.exists() or temporary.is_symlink():
        raise ValueError(f"{temporary}: leftover temporary file; inspect and remove it first")
    write_private(temporary, config)
    os.replace(temporary, path)  # Atomic: never leaves a half-written configuration.


def stale_binary():
    """True when Hub sources or web assets are newer than the release binary (mtime heuristic)."""
    built = BINARY.stat().st_mtime
    sources = [ROOT / "hub/Cargo.toml", ROOT / "hub/Cargo.lock", *(ROOT / "hub/src").rglob("*.rs")]
    return any(path.stat().st_mtime > built for path in sources if path.is_file())


def access_notice(directory, env):
    users = ", ".join(user["id"] for user in json.loads(env["AGENCT_USERS_JSON"]))
    port = env["AGENCT_BIND"].rsplit(":", 1)[1]
    return (f"Starting Agenct Hub (runtime={env['AGENCT_RUNTIME']}, loopback only).\n"
            f"  Open:   http://localhost:{port}\n"
            f"  Log in: paste a token for one of [{users}] from {directory / 'credentials.json'}\n"
            f"          (tokens are not printed; use a separate browser profile per user)\n"
            f"  Stop:   Ctrl+C")


def launch(directory, env):
    port = int(env["AGENCT_BIND"].rsplit(":", 1)[1])
    if not BINARY.is_file():
        raise ValueError("build first: cargo build --locked --release --manifest-path hub/Cargo.toml "
                         "(or use: python3 scripts/private-staging.py start)")
    if stale_binary():
        print("Warning: Hub sources are newer than the release binary; rebuild after pulling "
              "(private-staging.py start rebuilds automatically).", file=sys.stderr)
    if hub_running(directory):
        raise ValueError(f"a Hub is already running for this state directory; use http://localhost:{port} "
                         "or stop it (Ctrl+C in its terminal) first")
    if not port_available(port):
        raise ValueError(busy_port_message(port, directory))
    print(access_notice(directory, env), flush=True)
    os.umask(0o077)
    os.chdir(ROOT)
    os.execve(BINARY, [str(BINARY)], env)


def build():
    if shutil.which("cargo"):
        if BINARY.is_file():
            print("Checking the release Hub is up to date...", flush=True)
        else:
            print("Building the release Hub (first build downloads dependencies and takes a few minutes)...", flush=True)
        subprocess.run(BUILD, check=True)
    elif BINARY.is_file():
        print("cargo not found; using the existing release binary without rebuilding.", file=sys.stderr)
    else:
        raise ValueError("cargo not found and no release Hub binary exists; install Rust stable first")


def start(directory, users, port):
    if not (directory.exists() or directory.is_symlink()):
        if port is None:
            port = first_free_port()
            if port != DEFAULT_PORT:
                print(f"Port {DEFAULT_PORT} is in use; using free port {port} instead.")
        elif not port_available(port):
            raise ValueError(f"127.0.0.1:{port} is already in use by another program; choose another --port")
        initialize(directory, users, port)
        print(f"Initialized private state in {directory} (users: {', '.join(users)}).")
    elif port is not None:
        raise ValueError("--port only applies to a new state directory; use set-port to change an existing one")
    env = environment(directory)
    build()
    launch(directory, env)


def environment(directory):
    secure_path(directory, directory=True)
    config_path = directory / "config.json"
    secure_path(config_path)
    config = json.loads(config_path.read_text())
    if not isinstance(config, dict) or set(config) != {"version", "port", "runtime", "users"} or type(config["version"]) is not int or config["version"] != 1:
        raise ValueError("unsupported configuration schema")
    port = config["port"]
    valid_port(port)
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
    parser.add_argument("action", choices=("start", "init", "check", "run", "set-port"),
                        help="start = init if missing, build, then run (recommended)")
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--users", nargs="+", default=["alice", "bob"])
    parser.add_argument("--port", type=int, default=None, help=f"loopback port (default {DEFAULT_PORT})")
    args = parser.parse_args()
    # Do not resolve final component: lstat must detect directory symlinks.
    directory = Path(os.path.abspath(args.state_dir))
    if args.action == "start":
        start(directory, args.users, args.port)
        return
    if args.action == "init":
        port = DEFAULT_PORT if args.port is None else args.port
        initialize(directory, args.users, port)
        print("Private staging initialized. Credentials are in the owner-only credentials.json file; keep them private.")
        if not port_available(port):
            print("Warning: " + busy_port_message(port, directory), file=sys.stderr)
        return
    if args.action == "set-port":
        if args.port is None:
            raise ValueError("set-port requires --port")
        set_port(directory, args.port)
        print(f"Port set to {args.port}. Credentials and data were not changed.")
        return
    env = environment(directory)
    if args.action == "check":
        port = int(env["AGENCT_BIND"].rsplit(":", 1)[1])
        print(f"Configuration valid: loopback-only, runtime={env['AGENCT_RUNTIME']}. This is not a live service or provider check.")
        if not port_available(port):
            print("Warning: " + busy_port_message(port, directory), file=sys.stderr)
        return
    launch(directory, env)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError:
        print("Private staging refused: release build failed; see the cargo output above.", file=sys.stderr)
        sys.exit(1)
    except (ValueError, OSError, json.JSONDecodeError) as exc:
        print(f"Private staging refused: {exc}", file=sys.stderr)
        sys.exit(1)
