#!/usr/bin/env python3
"""Private, loopback-only staging. Never prints credentials or provisions services."""
import argparse
from contextlib import contextmanager, closing
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import stat
import sqlite3
import tempfile
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "hub/target/release/agenct-hub"
BUILD = ["cargo", "build", "--locked", "--release", "--manifest-path", str(ROOT / "hub/Cargo.toml")]
DEFAULT_PORT = 8080
RUNTIMES = ("mock", "dsh")
ADAPTER = ROOT / "adapters/dsh/adapter.py"
DSH_REQUIREMENTS = ROOT / "adapters/dsh/requirements.txt"
# Only these reach the managed adapter. Hub identity config never does; the provider key never reaches the Hub.
ADAPTER_ENV = ("PATH", "LANG", "LC_ALL", "DEEPSEEK_API_KEY", "AGENCT_DSH_MODEL")


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


def initialize(directory, users, port, runtime="mock"):
    if not users or len(set(users)) != len(users) or any(not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", user) for user in users):
        raise ValueError("supply distinct user IDs of 1–64 ASCII letters, digits, underscores or hyphens")
    valid_port(port)
    valid_runtime(runtime)
    # Fail closed on existing directories: never rotate credentials or overwrite data implicitly.
    directory.mkdir(mode=0o700)
    credentials = {user: secrets.token_urlsafe(32) for user in users}
    config = {
        "version": 1,
        "port": port,
        "runtime": runtime,
        "users": [{"id": user, "token_sha256": hashlib.sha256(token.encode()).hexdigest()} for user, token in credentials.items()],
    }
    write_private(directory / "config.json", config)
    write_private(directory / "credentials.json", credentials)


def valid_runtime(runtime):
    if runtime not in RUNTIMES:
        raise ValueError("runtime must explicitly be mock or dsh")


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


def update_config(directory, key, value):
    with state_lock(directory, "admin.lock"):
        _update_config(directory, key, value)


def _update_config(directory, key, value):
    environment(directory)  # Validate ownership, permissions and schema before rewriting.
    if hub_running(directory):
        # The running Hub keeps its old port/runtime; changing config now would make
        # later notices describe a process that does not exist.
        raise ValueError("a Hub is running for this state directory; stop it (Ctrl+C) before changing its configuration")
    path = directory / "config.json"
    config = json.loads(path.read_text())
    config[key] = value
    temporary = directory / "config.json.tmp"
    if temporary.exists() or temporary.is_symlink():
        raise ValueError(f"{temporary}: leftover temporary file; inspect and remove it first")
    write_private(temporary, config)
    os.replace(temporary, path)  # Atomic: never leaves a half-written configuration.


@contextmanager
def state_lock(directory, name):
    secure_path(directory, directory=True)
    path = directory / name
    if path.exists() or path.is_symlink():
        secure_path(path)
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "r+") as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            raise ValueError("state is busy; stop the Hub before restoring, or retry after the other administration command")
        yield


def replace_private(path, value):
    fd, name = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def change_user(directory, user, rotate=False):
    if not user or not re.fullmatch(r"[A-Za-z0-9_-]{1,64}", user):
        raise ValueError("--user must contain 1–64 ASCII letters, digits, underscores or hyphens")
    with state_lock(directory, "admin.lock"):
        environment(directory)
        config_path, credentials_path = directory / "config.json", directory / "credentials.json"
        secure_path(credentials_path)
        config, credentials = json.loads(config_path.read_text()), json.loads(credentials_path.read_text())
        members = {entry["id"] for entry in config["users"]}
        if (user in members) != rotate:
            raise ValueError("rotate-token requires an existing user; add-user requires a new user")
        if not isinstance(credentials, dict) or set(credentials) != members:
            raise ValueError("credentials do not match configured users")
        token = secrets.token_urlsafe(32)
        digest = hashlib.sha256(token.encode()).hexdigest()
        old_credentials = credentials.copy()
        credentials[user] = token
        if rotate:
            next(entry for entry in config["users"] if entry["id"] == user)["token_sha256"] = digest
        else:
            config["users"].append({"id": user, "token_sha256": digest})
        if len((json.dumps(config, indent=2) + "\n").encode()) > 131072:
            raise ValueError("private configuration exceeds 128 KiB")
        # Publish the secret before enabling it; the live Hub reads only config.json.
        replace_private(credentials_path, credentials)
        try:
            replace_private(config_path, config)
        except Exception:
            replace_private(credentials_path, old_credentials)
            raise


def sqlite_snapshot(source, destination):
    # SQLite's online backup includes committed WAL data and never copies a torn database.
    with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as src, closing(sqlite3.connect(destination)) as dst:
        src.backup(dst)
        if dst.execute("PRAGMA integrity_check").fetchone() != ("ok",):
            raise ValueError("database integrity check failed")


def backup_database(directory, destination):
    with state_lock(directory, "admin.lock"):
        _backup_database(directory, destination)


def _backup_database(directory, destination):
    environment(directory)
    source = directory / "hub.sqlite3"
    secure_path(source)
    # Exclusive creation prevents overwriting an earlier backup or following a symlink.
    fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(fd)
    try:
        sqlite_snapshot(source, destination)
    except Exception:
        destination.unlink()
        raise


def restore_database(directory, source):
    with state_lock(directory, "admin.lock"), state_lock(directory, "hub.lock"):
        environment(directory)
        secure_path(source)
        target = directory / "hub.sqlite3"
        if source.resolve() == target.resolve():
            raise ValueError("restore source must be a separate backup")
        fd, name = tempfile.mkstemp(prefix="restore-", suffix=".sqlite3", dir=directory)
        os.close(fd)
        temporary = Path(name)
        try:
            sqlite_snapshot(source, temporary)
            with closing(sqlite3.connect(temporary)) as db:
                tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
                if not {"sessions", "messages", "tasks", "proposals"}.issubset(tables):
                    raise ValueError("backup is not an Agenct database")
            # Remove stale sidecars while holding the same lock as the Hub.
            for suffix in ("-wal", "-shm"):
                target.with_name(target.name + suffix).unlink(missing_ok=True)
            os.replace(temporary, target)
        finally:
            temporary.unlink(missing_ok=True)


def set_port(directory, port):
    valid_port(port)
    update_config(directory, "port", port)


def set_runtime(directory, runtime):
    valid_runtime(runtime)
    update_config(directory, "runtime", runtime)


def stale_binary():
    """True when Hub sources or web assets are newer than the release binary (mtime heuristic)."""
    built = BINARY.stat().st_mtime
    sources = [ROOT / "hub/Cargo.toml", ROOT / "hub/Cargo.lock", *(ROOT / "hub/src").rglob("*.rs")]
    return any(path.stat().st_mtime > built for path in sources if path.is_file())


def access_notice(directory, env):
    users = ", ".join(user["id"] for user in json.loads(env["AGENCT_USERS_JSON"]))
    port = env["AGENCT_BIND"].rsplit(":", 1)[1]
    real = ("  Model:  REAL DeepSeek calls. Chat content is sent to the provider and billed to your key.\n"
            if env["AGENCT_RUNTIME"] == "dsh" else "")
    return (f"Starting Agenct Hub (runtime={env['AGENCT_RUNTIME']}, loopback only).\n" + real +
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
    if env["AGENCT_RUNTIME"] == "dsh" and not external_adapter(env):
        supervise_dsh(directory, env)
        return
    print(access_notice(directory, env), flush=True)
    os.umask(0o077)
    os.chdir(ROOT)
    os.execve(BINARY, [str(BINARY)], env)


def external_adapter(env):
    """Operator-managed adapter (the original DSH mode): both values supplied by the shell."""
    supplied = [bool(env.get(key)) for key in ("AGENCT_ADAPTER_URL", "AGENCT_ADAPTER_TOKEN")]
    if any(supplied) and not all(supplied):
        raise ValueError("set both AGENCT_ADAPTER_URL and AGENCT_ADAPTER_TOKEN for an external adapter, "
                         "or neither to let the launcher manage it")
    return all(supplied)


def dsh_python(directory):
    return directory / "dsh-venv/bin/python"


def dsh_ready(directory):
    """True when the private venv has exactly the pinned SDK and runtime."""
    python = dsh_python(directory)
    if not python.is_file():
        return False
    pinned = dict(line.split("==", 1) for line in DSH_REQUIREMENTS.read_text().split()
                  if "==" in line and not line.startswith("#"))
    probe = ("import json,sys\nfrom importlib.metadata import version,PackageNotFoundError\n"
             "out={}\nfor n in sys.argv[1:]:\n"
             "    try: out[n]=version(n)\n    except PackageNotFoundError: out[n]=None\nprint(json.dumps(out))")
    result = subprocess.run([str(python), "-I", "-c", probe, *pinned], capture_output=True, text=True,
                            env={"PATH": os.environ.get("PATH", os.defpath)})
    return result.returncode == 0 and json.loads(result.stdout) == pinned


def ensure_dsh(directory):
    """Create a private venv inside the state directory and install the pinned adapter dependencies."""
    if dsh_ready(directory):
        return
    if sys.version_info < (3, 11):
        raise ValueError("runtime=dsh needs Python 3.11+ to run this launcher (the adapter venv uses the same interpreter)")
    venv = directory / "dsh-venv"
    if not venv.exists():
        print("Creating the private DSH adapter environment (one time)...", flush=True)
        try:
            subprocess.run([sys.executable, "-m", "venv", str(venv)], check=True, capture_output=True)
        except subprocess.CalledProcessError:
            shutil.rmtree(venv, ignore_errors=True)
            raise ValueError("could not create a Python venv; on Debian/Ubuntu install it with: sudo apt install python3-venv")
    print("Installing pinned DSH SDK/runtime from adapters/dsh/requirements.txt...", flush=True)
    subprocess.run([str(venv / "bin/python"), "-m", "pip", "install", "--quiet", "--disable-pip-version-check",
                    "-r", str(DSH_REQUIREMENTS)], check=True)
    if not dsh_ready(directory):
        raise ValueError("DSH SDK/runtime versions do not match adapters/dsh/requirements.txt after install")


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def adapter_environment(token, port):
    env = {key: os.environ[key] for key in ADAPTER_ENV if os.environ.get(key)}
    env.update(AGENCT_ADAPTER_TOKEN=token, AGENCT_ADAPTER_PORT=str(port))
    return env


def wait_listening(port, process, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise ValueError("the DSH adapter exited during startup; check the messages above")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                return
        except OSError:
            time.sleep(0.1)
    raise ValueError("the DSH adapter did not start listening in time")


def stop(process, group=False):
    if process is None or process.poll() is not None:
        return
    try:
        if group:
            os.killpg(process.pid, signal.SIGTERM)
        else:
            process.send_signal(signal.SIGINT)  # The Hub shuts down gracefully on Ctrl+C.
        process.wait(timeout=10)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL) if group else process.kill()
        except ProcessLookupError:
            pass
        process.wait()


def require_provider_key(directory):
    if not os.environ.get("DEEPSEEK_API_KEY"):
        raise ValueError("runtime=dsh needs DEEPSEEK_API_KEY in this shell (real, billable provider calls). "
                         "It is passed only to the adapter process and never stored. To go back to the "
                         "offline demo: python3 scripts/private-staging.py set-runtime "
                         f"--state-dir {directory} --runtime mock")


def supervise_dsh(directory, env):
    """Run adapter + Hub together with a per-launch internal token. Never falls back to mock."""
    require_provider_key(directory)
    if not dsh_ready(directory):
        raise ValueError("DSH adapter environment missing or out of date; run private-staging.py start "
                         "(it installs the pinned SDK into the state directory)")
    # Fresh per launch: nothing to store, rotate or leak between runs.
    token, port = secrets.token_urlsafe(32), free_port()
    hub_env = {key: value for key, value in env.items() if key != "DEEPSEEK_API_KEY"}
    hub_env.update(AGENCT_ADAPTER_URL=f"http://127.0.0.1:{port}/v1/turn", AGENCT_ADAPTER_TOKEN=token)
    os.umask(0o077)
    adapter = hub = None
    adapter_died = False
    try:
        adapter = subprocess.Popen([str(dsh_python(directory)), "-I", str(ADAPTER)], cwd=str(ADAPTER.parent),
                                   env=adapter_environment(token, port), start_new_session=True)
        wait_listening(port, adapter)
        print(access_notice(directory, hub_env), flush=True)
        hub = subprocess.Popen([str(BINARY)], cwd=str(ROOT), env=hub_env)
        while hub.poll() is None and adapter.poll() is None:
            time.sleep(0.2)
        if adapter.poll() is not None and hub.poll() is None:
            adapter_died = True
            print("The DSH adapter stopped unexpectedly; stopping the Hub.", file=sys.stderr)
    except KeyboardInterrupt:
        pass
    finally:
        stop(hub)
        stop(adapter, group=True)
    if adapter_died:
        raise SystemExit(1)
    if hub is not None and hub.returncode not in (0, -signal.SIGINT):
        raise SystemExit(hub.returncode if hub.returncode > 0 else 1)


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


def start(directory, users, port, runtime=None):
    if not (directory.exists() or directory.is_symlink()):
        if port is None:
            port = first_free_port()
            if port != DEFAULT_PORT:
                print(f"Port {DEFAULT_PORT} is in use; using free port {port} instead.")
        elif not port_available(port):
            raise ValueError(f"127.0.0.1:{port} is already in use by another program; choose another --port")
        initialize(directory, users, port, runtime or "mock")
        print(f"Initialized private state in {directory} (users: {', '.join(users)}).")
    elif port is not None:
        raise ValueError("--port only applies to a new state directory; use set-port to change an existing one")
    elif runtime is not None:
        raise ValueError("--runtime only applies to a new state directory; use set-runtime to change an existing one")
    env = environment(directory)
    if env["AGENCT_RUNTIME"] == "dsh" and not external_adapter(env):
        require_provider_key(directory)  # Fail before any build/install work.
        ensure_dsh(directory)
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
    valid_runtime(config["runtime"])
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
               AGENCT_RUNTIME=config["runtime"], AGENCT_USERS_JSON=json.dumps(users),
               AGENCT_PRIVATE_CONFIG=str(config_path))
    return env


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("start", "init", "check", "run", "set-port", "set-runtime", "add-user", "rotate-token", "backup", "restore"),
                        help="start = init if missing, build, then run (recommended)")
    parser.add_argument("--state-dir", required=True, type=Path)
    parser.add_argument("--users", nargs="+", default=["alice", "bob"])
    parser.add_argument("--port", type=int, default=None, help=f"loopback port (default {DEFAULT_PORT})")
    parser.add_argument("--runtime", choices=RUNTIMES, default=None,
                        help="mock (offline demo, default) or dsh (real DeepSeek calls; needs DEEPSEEK_API_KEY)")
    parser.add_argument("--user", help="identity for add-user or rotate-token")
    parser.add_argument("--backup-file", type=Path, help="new backup destination or restore source")
    args = parser.parse_args()
    # Do not resolve final component: lstat must detect directory symlinks.
    directory = Path(os.path.abspath(args.state_dir))
    if args.action in ("add-user", "rotate-token"):
        change_user(directory, args.user, rotate=args.action == "rotate-token")
        print("User credentials updated. Read the owner-only credentials.json; the running Hub applies this on its next authenticated request.")
        return
    if args.action in ("backup", "restore"):
        if args.backup_file is None:
            raise ValueError("backup and restore require --backup-file")
        path = Path(os.path.abspath(args.backup_file))
        (backup_database if args.action == "backup" else restore_database)(directory, path)
        print("Database " + ("backed up consistently." if args.action == "backup" else "restored. Start the Hub to resume."))
        return
    if args.action == "start":
        start(directory, args.users, args.port, args.runtime)
        return
    if args.action == "init":
        port = DEFAULT_PORT if args.port is None else args.port
        initialize(directory, args.users, port, args.runtime or "mock")
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
    if args.action == "set-runtime":
        if args.runtime is None:
            raise ValueError("set-runtime requires --runtime mock|dsh")
        set_runtime(directory, args.runtime)
        print(f"Runtime set to {args.runtime}. Credentials and data were not changed.")
        return
    env = environment(directory)
    if args.action == "check":
        port = int(env["AGENCT_BIND"].rsplit(":", 1)[1])
        print(f"Configuration valid: loopback-only, runtime={env['AGENCT_RUNTIME']}. This is not a live service or provider check.")
        if not port_available(port):
            print("Warning: " + busy_port_message(port, directory), file=sys.stderr)
        if env["AGENCT_RUNTIME"] == "dsh" and not external_adapter(env):
            print("DSH adapter environment: " + ("ready" if dsh_ready(directory) else "missing (start installs it)"))
            print("DEEPSEEK_API_KEY in this shell: " + ("set" if os.environ.get("DEEPSEEK_API_KEY") else "NOT set"))
        return
    launch(directory, env)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError:
        print("Private staging refused: a build/install step failed; see the output above.", file=sys.stderr)
        sys.exit(1)
    except (ValueError, OSError, json.JSONDecodeError, sqlite3.Error) as exc:
        print(f"Private staging refused: {exc}", file=sys.stderr)
        sys.exit(1)
