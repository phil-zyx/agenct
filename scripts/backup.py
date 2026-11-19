#!/usr/bin/env python3
"""Minimalist Agenct v2 state backup & restore (SQLite online snapshot + attachments)."""
from __future__ import annotations

import argparse
import os
import shutil
import sqlite3
import tempfile
import zipfile
from contextlib import closing
from pathlib import Path


def backup(data_dir: Path, archive: Path) -> None:
    if archive.exists():
        raise SystemExit(f"Archive already exists: {archive}")
    db_path = data_dir / "agenct.sqlite3"
    if not db_path.is_file():
        raise SystemExit(f"Database not found: {db_path}")

    archive.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="agenct-bak-") as tmp:
        snap = Path(tmp) / "agenct.sqlite3"
        with closing(sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)) as src, closing(
            sqlite3.connect(snap)
        ) as dst:
            src.backup(dst)
            if dst.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                raise SystemExit("SQLite integrity_check failed")

        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            zf.write(snap, "agenct.sqlite3")
            att_dir = data_dir / "attachments"
            if att_dir.is_dir():
                for item in sorted(att_dir.iterdir()):
                    if item.is_file() and item.name.endswith(".bin"):
                        zf.write(item, f"attachments/{item.name}")
    os.chmod(archive, 0o600)
    print(f"Backup created: {archive}")


def restore(archive: Path, target_dir: Path) -> None:
    if target_dir.exists():
        raise SystemExit(f"Restore target must not exist yet: {target_dir}")
    target_dir.mkdir(mode=0o700, parents=True)
    try:
        with zipfile.ZipFile(archive, "r") as zf:
            for member in zf.namelist():
                if member.startswith("/") or ".." in Path(member).parts:
                    raise SystemExit(f"Unsafe archive member: {member}")
            zf.extractall(target_dir)
        db_path = target_dir / "agenct.sqlite3"
        with closing(sqlite3.connect(db_path)) as conn:
            if conn.execute("PRAGMA integrity_check").fetchone()[0] != "ok":
                raise SystemExit("Restored DB failed integrity_check")
            # Fail any in-flight node leases on restored instance.
            conn.execute(
                "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL "
                "WHERE status IN ('queued','running')"
            )
            conn.commit()
    except BaseException:
        shutil.rmtree(target_dir, ignore_errors=True)
        raise
    print(f"Restored into: {target_dir}")


def main() -> None:
    p = argparse.ArgumentParser(description="Agenct v2 backup & restore")
    sub = p.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("backup")
    b.add_argument("--data-dir", required=True, type=Path)
    b.add_argument("--archive", required=True, type=Path)
    r = sub.add_parser("restore")
    r.add_argument("--archive", required=True, type=Path)
    r.add_argument("--data-dir", required=True, type=Path)
    args = p.parse_args()
    if args.cmd == "backup":
        backup(args.data_dir, args.archive)
    else:
        restore(args.archive, args.data_dir)


if __name__ == "__main__":
    main()
