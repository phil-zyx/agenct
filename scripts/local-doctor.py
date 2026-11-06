#!/usr/bin/env python3
"""Read-only local prerequisites. No installs, secrets, subprocesses or network checks."""
import argparse
import os
from pathlib import Path
import platform
import re
import shutil
import sys

ROOT = Path(__file__).resolve().parents[1]
LIMITS = ("This checks local prerequisites only, not a running Hub, browser/WSL forwarding, "
          "live DSH/Claude Code, effective permissions, or security. No tools were executed.")


def host_kind(system, release, env):
    if system != "Linux":
        return "non-linux"
    release = release.lower()
    if "microsoft" in release and "wsl2" in release:
        return "wsl2"
    if re.fullmatch(r"4\.4\..*-microsoft", release):
        return "wsl1"
    if "microsoft" in release or "wsl" in release or any(
            name in env for name in ("WSL_DISTRO_NAME", "WSL_INTEROP")):
        return "wsl-unknown"
    return "linux"


def unescape_mount(value):
    # mountinfo encodes spaces, tabs, newlines and backslashes as octal escapes.
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), value)


def filesystem_kind(path, mountinfo):
    """Best-effort mount classification, not an ACL/ownership security check."""
    resolved = path.resolve()
    # This fallback also works when /proc/self/mountinfo is unavailable.
    for candidate in (path.absolute(), resolved):
        if re.match(r"^/mnt/[a-zA-Z](?:/|$)", str(candidate)):
            return "windows"
    matches = []
    for line in (mountinfo or "").splitlines():
        fields = line.split()
        try:
            separator = fields.index("-")
            mount = Path(unescape_mount(fields[4]))
            fs_type, _, options = fields[separator + 1:separator + 4]
        except (ValueError, IndexError):
            continue
        if mount == resolved or mount in resolved.parents:
            matches.append((len(mount.parts), fs_type, options))
    if not matches:
        return "unknown"
    _, fs_type, options = max(matches, key=lambda match: match[0])
    if fs_type in {"drvfs", "ntfs", "ntfs3", "fuseblk", "cifs", "smb3"} or (
            fs_type == "9p" and "aname=drvfs" in options.lower()):
        return "windows"
    if fs_type == "9p":
        return "unknown"
    return "other"


def inspect_local(root, state, *, system, release, env, python_version, which, mountinfo):
    results = []

    def add(level, message):
        results.append((level, message))

    host = host_kind(system, release, env)
    if host == "non-linux":
        add("FAIL", "This quickstart targets Linux/WSL2. Run it inside your WSL Linux terminal.")
    elif host == "wsl1":
        add("FAIL", "Kernel resembles WSL1. Confirm with Windows `wsl --list --verbose`; this guide targets WSL2.")
    elif host == "wsl2":
        add("NOTE", "WSL2 kernel markers found (heuristic). Confirm version with Windows `wsl --list --verbose`.")
    elif host == "wsl-unknown":
        add("WARN", "WSL markers found, but version is unknown. Confirm WSL2 with Windows `wsl --list --verbose`.")
    else:
        add("NOTE", "Ordinary Linux detected; this is not evidence of a WSL test.")

    if tuple(python_version[:2]) < (3, 9):
        add("FAIL", "Python 3.9+ is required by the local launcher; DSH separately needs Python 3.11+.")
    else:
        add("PASS", "Running Python supports the mock launcher (3.9+).")
        if tuple(python_version[:2]) < (3, 11):
            add("NOTE", "Optional DSH adapter needs a separate Python 3.11+ environment.")

    binary = root / "hub/target/release/agenct-hub"
    built = binary.is_file() and os.access(binary, os.X_OK)
    for name in ("git", "cargo", "rustc", "node", "npm"):
        if which(name):
            add("PASS", f"{name} is on PATH; version and functionality were not checked.")
        else:
            required = name in ("cargo", "rustc") and not built
            purpose = "build" if name in ("cargo", "rustc") else "pull/update" if name == "git" else "UI tests (Node 22+)"
            add("FAIL" if required else "WARN", f"{name} is missing from PATH; needed for {purpose}.")
    if built:
        add("PASS", "Release Hub file is executable. Its source revision/compatibility were not verified; rebuild after pull.")
    else:
        add("FAIL", "Release Hub missing/not executable. Build: cargo build --locked --release --manifest-path hub/Cargo.toml")
    if all((root / name).is_file() for name in ("hub/web/index.html", "hub/web/client.js", "scripts/private-staging.py")):
        add("PASS", "Launcher and Hub UI files are present.")
    else:
        add("FAIL", "Checkout is missing the launcher or Hub UI assets. Use a complete checkout.")

    for label, path in (("Checkout", root), ("Private state", state)):
        kind = filesystem_kind(path, mountinfo)
        if kind == "windows":
            add("FAIL" if label == "Private state" else "WARN",
                f"{label} appears on a Windows/shared mount. Use the WSL Linux filesystem under $HOME; chmod alone is not an ACL guarantee.")
        elif kind == "unknown":
            add("WARN", f"{label} filesystem could not be classified. Verify it is on native Linux storage, not DrvFS.")
        else:
            add("NOTE", f"{label} mount is not identified as Windows storage; permissions and trust were not verified.")
    resolved_root, resolved_state = root.resolve(), state.resolve()
    if resolved_root == resolved_state or resolved_root in resolved_state.parents:
        add("FAIL", "Keep private state outside the checkout, in a trusted owner-only directory.")
    if state.is_symlink():
        add("FAIL", "Private state must not be a symlink; the existing launcher rejects it.")
    elif state.exists():
        add("NOTE", "State path already exists. Do not reinitialize; use private-staging.py check before run. Contents were not read.")
    elif not state.parent.is_dir():
        add("FAIL", "State parent directory is missing. Choose an existing trusted private Linux directory.")
    else:
        add("NOTE", "State path does not exist; initialize once with private-staging.py. Nothing was created.")
    inherited = [name for name in ("AGENCT_OIDC_CONFIG_FILE", "AGENCT_BROWSER_OIDC_CONFIG_FILE", "AGENCT_WORKER_SCOPES_FILE") if name in env]
    if inherited:
        add("WARN", "Inherited optional OIDC/browser/worker configuration: " + ", ".join(inherited)
            + ". Review/remove it for a simple mock run; values were not read or printed.")
    add("NOTE", LIMITS)
    return results


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--state-dir", type=Path, default=Path.home() / "agenct-private")
    args = parser.parse_args(argv)
    try:
        try:
            mountinfo = Path("/proc/self/mountinfo").read_text() if platform.system() == "Linux" else None
        except OSError:
            mountinfo = None
        results = inspect_local(ROOT, args.state_dir.expanduser().absolute(),
                                system=platform.system(), release=platform.release(),
                                env=os.environ, python_version=sys.version_info,
                                which=shutil.which, mountinfo=mountinfo)
    except (OSError, RuntimeError):
        # Avoid echoing path/environment content from filesystem exceptions.
        print("[FAIL] Could not inspect local paths. Check access and symlink loops. No changes made.")
        print("[NOTE] " + LIMITS)
        return 1
    for level, message in results:
        print(f"[{level}] {message}")
    print("Read-only checks complete. Exit 0 means no listed blocker, not runtime readiness.")
    return int(any(level == "FAIL" for level, _ in results))


if __name__ == "__main__":
    sys.exit(main())
