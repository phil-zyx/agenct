"""Durable cleanup accounting for trusted, fixed benign fixtures on Linux.

The operator supplies a private host-owned directory on a local durable filesystem.
This is not a general container janitor, production executor, or lease service.
"""
import fcntl
import os
from pathlib import Path
import re
import sqlite3
import stat


class JournalRejected(RuntimeError):
    pass


class CleanupPending(JournalRejected):
    pass


def _match(pattern, value):
    return isinstance(value, str) and re.fullmatch(pattern, value) is not None


class CleanupJournal:
    """Hold the exclusive lock for the entire supervisor/recovery lifetime."""
    def __init__(self, directory):
        self.connection = None
        self.lock = None
        directory = Path(directory)
        directory.mkdir(mode=0o700, parents=False, exist_ok=True)
        info = directory.lstat()
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid()
                or info.st_mode & 0o077):
            raise JournalRejected('journal requires a private owned directory')
        try:
            fd = os.open(directory / 'lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
            self.lock = os.fdopen(fd, 'r+b')
            info = os.fstat(fd)
            if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1 or info.st_uid != os.getuid():
                raise JournalRejected('invalid journal lock')
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError as error:
                raise JournalRejected('another supervisor owns this journal') from error
            path = directory / 'cleanup.sqlite3'
            if path.exists() or path.is_symlink():
                info = path.lstat()
                if (not stat.S_ISREG(info.st_mode) or info.st_nlink != 1
                        or info.st_uid != os.getuid()):
                    raise JournalRejected('invalid journal database')
            self.connection = sqlite3.connect(path, timeout=0)
            os.chmod(path, 0o600)
            self.connection.execute('PRAGMA journal_mode=DELETE')
            self.connection.execute('PRAGMA synchronous=FULL')
            self.connection.execute('CREATE TABLE IF NOT EXISTS cleanup ('
                                    'name TEXT PRIMARY KEY, image TEXT NOT NULL, '
                                    'snapshot TEXT NOT NULL, container_id TEXT, '
                                    'created INTEGER NOT NULL DEFAULT 0)')
            self.connection.commit()
            # Persist the directory entry as well as SQLite's transaction.
            fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
            try:
                os.fsync(fd)
            finally:
                os.close(fd)
            self.pending()  # Reject malformed stored rows before any daemon action.
        except BaseException:
            self.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()

    def close(self):
        if self.connection is not None:
            self.connection.close()
            self.connection = None
        if self.lock is not None:
            self.lock.close()
            self.lock = None

    def pending(self):
        rows = self.connection.execute('SELECT name,image,snapshot,container_id,created FROM cleanup').fetchall()
        if len(rows) > 100:
            raise JournalRejected('journal record limit exceeded')
        for name, image, snapshot, container_id, created in rows:
            if (not _match(r'agenct-stage-[0-9a-f]{32}', name)
                    or not _match(r'sha256:[0-9a-f]{64}', image)
                    or not _match(r'[0-9a-f]{64}', snapshot)
                    or (container_id is not None and not _match(r'[0-9a-f]{64}', container_id))
                    or type(created) is not int or created not in (0, 1)):
                raise JournalRejected('invalid cleanup record')
        return rows

    def register(self, name, image, snapshot):
        if self.pending():
            raise CleanupPending('reconcile pending cleanup before creating another fixture')
        with self.connection:
            self.connection.execute('INSERT INTO cleanup(name,image,snapshot) VALUES(?,?,?)',
                                    (name, image, snapshot))
            self.pending()

    def cancel_unissued(self, name):
        """Caller guarantees create was never invoked while holding this lock."""
        with self.connection:
            changed = self.connection.execute('DELETE FROM cleanup WHERE name=? '
                                              'AND container_id IS NULL AND created=0',
                                              (name,)).rowcount
            if changed != 1:
                raise JournalRejected('cannot discard a possibly issued create')

    def created(self, name):
        with self.connection:
            self.connection.execute('UPDATE cleanup SET created=1 WHERE name=?', (name,))

    def bind(self, name, container_id):
        if not _match(r'[0-9a-f]{64}', container_id):
            raise JournalRejected('invalid container identity')
        with self.connection:
            changed = self.connection.execute('UPDATE cleanup SET container_id=? WHERE name=? '
                                              'AND (container_id IS NULL OR container_id=?)',
                                              (container_id, name, container_id)).rowcount
            if changed != 1:
                raise JournalRejected('container identity changed or registration missing')

    def reconcile(self, engine):
        """Only remove verified IDs; lookup returns None ONLY for definitive absence.

        The trusted engine must bound every call. A missing uncertain-create intent
        stays pending: a timed-out daemon create might still finish later.
        """
        failures = []
        for name, image, snapshot, container_id, created in self.pending():
            try:
                observed = engine.lookup(container_id or name, timeout=5.0)
                if observed is None:
                    if container_id is None and not created:
                        raise CleanupPending('unsettled create intent is absent but may still complete')
                else:
                    actual_id = observed.get('Id')
                    labels = observed.get('Config', {}).get('Labels', {})
                    if (not _match(r'[0-9a-f]{64}', actual_id)
                            or container_id is not None and actual_id != container_id
                            or observed.get('Name') != '/' + name
                            or observed.get('Image') != image
                            or labels.get('agenct.stage') != name
                            or labels.get('agenct.snapshot') != snapshot):
                        raise CleanupPending('daemon resource does not match registered identity')
                    self.bind(name, actual_id)
                    if engine.remove(actual_id, timeout=5.0) is not True:
                        raise CleanupPending('container removal not confirmed')
                    if engine.lookup(actual_id, timeout=5.0) is not None:
                        raise CleanupPending('container still present after removal')
                with self.connection:
                    self.connection.execute('DELETE FROM cleanup WHERE name=?', (name,))
            except Exception as error:
                failures.append(type(error).__name__)
        if failures:
            raise CleanupPending('cleanup pending: ' + ', '.join(failures))
