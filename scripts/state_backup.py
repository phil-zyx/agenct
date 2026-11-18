#!/usr/bin/env python3
"""Offline, verified application-state archives. Restore only into a new directory."""
import argparse
from contextlib import contextmanager, closing
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import sqlite3
import stat
import tempfile
import zipfile

FILES = frozenset(('config.json', 'credentials.json', 'launch.json', 'browser-oidc.json',
                   'client-secret', 'tenants.json', 'automation_state.json', 'worker-scopes.json',
                   'repositories.json', 'repository-gate.json', 'Caddyfile'))
MAX_FILE = 2 * 1024**3
MAX_TOTAL = 8 * 1024**3


def regular(path):
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError('state entries must be regular files owned by this user')
    return info


@contextmanager
def locked(directory):
    info = directory.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError('state directory must be owned by this user and not a symlink')
    handles = []
    try:
        for name in ('admin.lock', 'hub.lock'):
            path = directory / name
            if path.exists() or path.is_symlink():
                regular(path)
            handle = os.fdopen(os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600), 'r+')
            handles.append(handle)
            try:
                fcntl.flock(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError('state is busy; stop the Hub before taking a complete backup') from None
        yield
    finally:
        for handle in reversed(handles):
            handle.close()


def db_check(path):
    with closing(sqlite3.connect(path.as_uri() + '?mode=ro', uri=True)) as db:
        if db.execute('PRAGMA integrity_check').fetchone() != ('ok',):
            raise ValueError('database integrity check failed')
        tables = {r[0] for r in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
        if not {'sessions', 'messages', 'tasks', 'proposals'}.issubset(tables):
            raise ValueError('not an Agenct database')
        if db.execute('PRAGMA foreign_key_check').fetchone() is not None:
            raise ValueError('database foreign key check failed')


def allowed(name):
    p = PurePosixPath(name)
    return (name == p.as_posix() and bool(p.parts) and not p.is_absolute()
            and '..' not in p.parts and '\\' not in name
            and not any(part.startswith('.') or part.endswith(('.lock', '-wal', '-shm')) for part in p.parts)
            and name != 'manifest.json')



def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(block)
    return h.hexdigest()


def create(directory, destination):
    directory, destination = Path(os.path.abspath(directory)), Path(os.path.abspath(destination))
    with locked(directory), tempfile.TemporaryDirectory(prefix='agenct-snapshot-') as scratch:
        stage = Path(scratch)
        source = directory / 'hub.sqlite3'
        regular(source)
        with closing(sqlite3.connect(source.as_uri() + '?mode=ro', uri=True)) as src, closing(sqlite3.connect(stage / 'hub.sqlite3')) as dst:
            src.backup(dst)
        db_check(stage / 'hub.sqlite3')
        names = ['hub.sqlite3']
        for name in sorted(FILES):
            path = directory / name
            if path.exists() or path.is_symlink():
                regular(path)
                names.append(name)
        attachments = directory / 'attachments'
        if attachments.exists() or attachments.is_symlink():
            if not stat.S_ISDIR(attachments.lstat().st_mode):
                raise ValueError('attachments must be a directory, not a symlink')
            for path in sorted(attachments.iterdir()):
                regular(path)
                name = path.relative_to(directory).as_posix()
                if not allowed(name) or path.suffix != '.json':
                    raise ValueError('unexpected attachment entry')
                names.append(name)
        external_files = []
        launch = directory / 'launch.json'
        if launch.exists():
            configuration = json.loads(launch.read_text())
            if configuration.get('AGENCT_DATABASE', str(source)) != str(source):
                raise ValueError('complete backup requires the launch database to be state-dir/hub.sqlite3')
            for key, value in configuration.items():
                if not (key.endswith('_FILE') or key == 'AGENCT_PRIVATE_CONFIG') or not isinstance(value, str):
                    continue
                referenced = Path(value)
                try:
                    relative = referenced.relative_to(directory)
                except ValueError:
                    external_files.append(value)
                    continue
                name = relative.as_posix()
                if not allowed(name):
                    raise ValueError('unsupported local configuration path')
                current = directory
                for part in relative.parts:
                    current = current / part
                    if current.is_symlink():
                        raise ValueError('configuration references must not follow symlinks')
                regular(referenced)
                if name not in names:
                    names.append(name)
        if not any(name in names for name in ('config.json', 'launch.json')):
            raise ValueError('missing application launch configuration')
        entries = {}
        total = 0
        for name in names:
            path = stage / name if name == 'hub.sqlite3' else directory / name
            size = path.stat().st_size
            total += size
            if size > MAX_FILE or total > MAX_TOTAL:
                raise ValueError('state exceeds archive size limit')
            entries[name] = {'size': size, 'sha256': digest(path)}
        manifest = {'format': 'agenct-state-v1', 'source_directory': str(directory), 'files': entries, 'external_files': external_files,
                    'excludes': ['runtime binaries/venvs', 'logs', 'external files, node-local journals and environment secrets']}
        destination.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        fd = os.open(destination, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        try:
            with os.fdopen(fd, 'wb') as output:
                with zipfile.ZipFile(output, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
                    archive.writestr('manifest.json', json.dumps(manifest, sort_keys=True))
                    for name in names:
                        archive.write(stage / name if name == 'hub.sqlite3' else directory / name, name)
                output.flush()
                os.fsync(output.fileno())
            # Re-open and verify actual archived bytes before reporting success.
            with tempfile.TemporaryDirectory(prefix='agenct-verify-') as check:
                unpack(destination, Path(check))
        except BaseException:
            destination.unlink(missing_ok=True)
            raise


def unpack(source, stage):
    regular(source)
    with zipfile.ZipFile(source) as archive:
        infos = archive.infolist()
        names = [entry.filename for entry in infos]
        if len(names) != len(set(names)) or names.count('manifest.json') != 1:
            raise ValueError('duplicate entries or missing manifest')
        if archive.getinfo('manifest.json').file_size > 4 * 1024 * 1024:
            raise ValueError('manifest too large')
        manifest = json.loads(archive.read('manifest.json'))
        entries = manifest.get('files')
        if manifest.get('format') != 'agenct-state-v1' or not isinstance(entries, dict):
            raise ValueError('unsupported state archive')
        if set(names) != set(entries) | {'manifest.json'} or 'hub.sqlite3' not in entries or not any(name in entries for name in ('config.json', 'launch.json')):
            raise ValueError('archive does not match manifest')
        total = 0
        for name, record in entries.items():
            if not allowed(name) or not isinstance(record, dict):
                raise ValueError('invalid archive path or record')
            info = archive.getinfo(name)
            mode = info.external_attr >> 16
            if stat.S_ISLNK(mode) or info.is_dir() or info.file_size > MAX_FILE:
                raise ValueError('invalid archive entry')
            total += info.file_size
            if total > MAX_TOTAL or record.get('size') != info.file_size:
                raise ValueError('invalid archive size')
            path = stage / name
            path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            with archive.open(name) as src, os.fdopen(os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600), 'wb') as dst:
                shutil.copyfileobj(src, dst, 1024 * 1024)
            if digest(path) != record.get('sha256'):
                raise ValueError('archive checksum mismatch')
    db_check(stage / 'hub.sqlite3')
    return manifest


def restore(source, destination, runtime_directory=None):
    source, destination = Path(os.path.abspath(source)), Path(os.path.abspath(destination))
    if destination.exists() or destination.is_symlink():
        raise ValueError('restore requires a new state directory; existing data is never overwritten')
    with tempfile.TemporaryDirectory(prefix='.agenct-restore-', dir=destination.parent) as scratch:
        stage = Path(scratch)
        manifest = unpack(source, stage)
        original = manifest.get('source_directory')
        if not isinstance(original, str) or not original.startswith('/'):
            raise ValueError('invalid original state directory')
        # Restoring an older snapshot must not replay physical work performed since it.
        with closing(sqlite3.connect(stage / 'hub.sqlite3')) as db:
            tables = {row[0] for row in db.execute("SELECT name FROM sqlite_master WHERE type='table'")}
            if 'node_assignments' in tables:
                db.execute("UPDATE node_assignments SET status='failed' WHERE status IN ('queued','running')")
            if 'browser_sessions' in tables:
                db.execute("DELETE FROM browser_sessions")
            if 'automation_state' in tables:
                row = db.execute("SELECT state_json FROM automation_state WHERE id = 1").fetchone()
                if row and row[0]:
                    data = json.loads(row[0])
                    for cron in data.get('crons', []):
                        cron['enabled'] = False
                    for config in data.get('gitlab_configs', {}).values():
                        config['auto_poll_todos'] = False
                    db.execute(
                        "UPDATE automation_state SET state_json = ? WHERE id = 1",
                        (json.dumps(data),),
                    )
            db.commit()
        automation = stage / 'automation_state.json'
        if automation.exists():
            data = json.loads(automation.read_text())
            for cron in data.get('crons', []):
                cron['enabled'] = False
            for config in data.get('gitlab_configs', {}).values():
                config['auto_poll_todos'] = False
            automation.write_text(json.dumps(data, indent=2) + '\n')
        runtime_directory = Path(runtime_directory or destination)
        if not runtime_directory.is_absolute():
            raise ValueError('runtime state directory must be absolute')
        # Only relocate launch variables pointing inside the captured state directory.
        launch = stage / 'launch.json'
        if launch.exists():
            data = json.loads(launch.read_text())
            for key, value in data.items():
                if isinstance(value, str) and value.startswith(original.rstrip('/') + '/'):
                    data[key] = str(runtime_directory / value[len(original.rstrip('/')) + 1:])
            launch.write_text(json.dumps(data, indent=2) + '\n')
        # Exclusive directory creation prevents replacing even an empty existing directory.
        destination.mkdir(mode=0o700)
        marker = destination / '.restore-in-progress'
        marker.touch(mode=0o600)
        try:
            for item in stage.iterdir():
                shutil.move(str(item), str(destination / item.name))
            for path in destination.rglob('*'):
                if path.is_file():
                    with path.open('rb') as stream:
                        os.fsync(stream.fileno())
            marker.unlink()
            fd = os.open(destination, os.O_RDONLY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
        except BaseException:
            shutil.rmtree(destination)
            raise


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('action', choices=('create', 'restore', 'verify'))
    parser.add_argument('--archive', required=True, type=Path)
    parser.add_argument('--state-dir', type=Path, help='source for create; new destination for restore')
    parser.add_argument('--runtime-state-dir', type=Path, help='optional final runtime mount path when restoring')
    args = parser.parse_args()
    if args.action == 'verify':
        with tempfile.TemporaryDirectory(prefix='agenct-verify-') as scratch:
            unpack(Path(os.path.abspath(args.archive)), Path(scratch))
    else:
        if args.state_dir is None:
            parser.error('--state-dir is required')
        if args.action == 'create':
            create(args.state_dir, args.archive)
        else:
            restore(args.archive, args.state_dir, args.runtime_state_dir)
    print('Application state ' + {'create': 'backed up', 'restore': 'restored', 'verify': 'verified'}[args.action] + '. External files and environment secrets are not included.')


if __name__ == '__main__':
    main()
