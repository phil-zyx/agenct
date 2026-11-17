#!/usr/bin/env python3
"""Container entrypoint for the intranet GitLab-login deployment.

  run     first start: generate private config in /data from environment; then start Hub
  backup  consistent SQLite snapshot: entrypoint.py backup /data/backups/name.sqlite3

Never prints credentials. Configuration is generated once; later changes are
explicit edits of /data/browser-oidc.json or /data/launch.json.
"""
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
from contextlib import closing
from pathlib import Path

ROOT = Path('/opt/agenct')
DATA = Path(os.environ.get('AGENCT_DATA_DIR', '/data'))
MODEL_DIR = Path('/opt/agenct-model')


def need(name):
    value = os.environ.get(name, '').strip()
    if not value:
        raise SystemExit(f'missing required environment variable {name}')
    return value


def configure():
    if (DATA / 'launch.json').exists():
        return
    origin = need('AGENCT_PUBLIC_ORIGIN').rstrip('/')
    source = {
        'GITLAB_OAUTH_BASE_URL': os.environ.get('GITLAB_URL', 'https://git.example.com').rstrip('/'),
        'GITLAB_OAUTH_CLIENT_ID': need('GITLAB_OAUTH_CLIENT_ID'),
        'GITLAB_OAUTH_CLIENT_SECRET': need('GITLAB_OAUTH_CLIENT_SECRET'),
        'GITLAB_OAUTH_REDIRECT_URI': origin + '/identity/v1/auth/oidc/gitlab/callback',
    }
    os.chmod(DATA, 0o700)
    with tempfile.NamedTemporaryFile('w', dir='/tmp', delete=True) as handle:
        os.chmod(handle.name, 0o600)
        handle.write(''.join(f'{k}={v}\n' for k, v in source.items()))
        handle.flush()
        subprocess.run([sys.executable, str(ROOT / 'scripts/configure-gitlab-login.py'),
                        '--source-env', handle.name, '--auto-signup', '--reuse-registered-callback',
                        '--state-dir', str(DATA)], check=True)
    oidc_path = DATA / 'browser-oidc.json'
    oidc = json.loads(oidc_path.read_text())
    ttl = int(os.environ.get('AGENCT_SESSION_TTL_SECONDS', '28800'))
    oidc['session_ttl_seconds'] = ttl
    oidc_path.write_text(json.dumps(oidc, indent=2))
    (DATA / 'Caddyfile').unlink(missing_ok=True)
    print(f'Generated GitLab login configuration for {origin} (session {ttl}s).', flush=True)


def run():
    configure()
    os.umask(0o077)
    command = [sys.executable, str(ROOT / 'scripts/start-gitlab-login.py'), '--state-dir', str(DATA)]
    runtime = os.environ.get('AGENCT_MODEL_RUNTIME', 'dsh')
    if runtime == 'dsh':
        command += ['--model-state-dir', str(MODEL_DIR)]
    elif runtime != 'mock':
        raise SystemExit('AGENCT_MODEL_RUNTIME must be dsh or mock')
    os.chdir(ROOT)
    os.execv(sys.executable, command)


def backup(destination):
    target = Path(destination)
    if not target.is_absolute() or target.exists():
        raise SystemExit('backup destination must be a new absolute path')
    target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    os.close(fd)
    try:
        with closing(sqlite3.connect(f'file:{DATA / "hub.sqlite3"}?mode=ro', uri=True)) as src, \
             closing(sqlite3.connect(target)) as dst:
            src.backup(dst)
            if dst.execute('PRAGMA integrity_check').fetchone()[0] != 'ok':
                raise SystemExit('backup integrity check failed')
    except BaseException:
        target.unlink(missing_ok=True)
        raise
    print('Database backed up consistently to', target)


if __name__ == '__main__':
    action = sys.argv[1] if len(sys.argv) > 1 else 'run'
    if action == 'run':
        run()
    elif action == 'backup' and len(sys.argv) == 3:
        backup(sys.argv[2])
    else:
        raise SystemExit(__doc__)
