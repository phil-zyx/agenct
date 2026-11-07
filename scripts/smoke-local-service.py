#!/usr/bin/env python3
"""Local user-systemd crash restart + isolated enable-link check; never reboots the host."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.request
import uuid

ROOT = Path(__file__).resolve().parents[1]


def run(*args):
    return subprocess.run(args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, check=True).stdout.strip()


def main():
    with tempfile.TemporaryDirectory(prefix='agenct-service-smoke-') as temporary:
        folder = Path(temporary)
        fake_root = folder/'enable-root'
        unit_dir = fake_root/'etc/systemd/system'
        unit_dir.mkdir(parents=True)
        shutil.copyfile(ROOT/'deploy/agenct-private.service.example', unit_dir/'agenct-private.service')
        run('systemctl', '--root='+str(fake_root), 'enable', 'agenct-private.service')
        assert (unit_dir/'multi-user.target.wants/agenct-private.service').is_symlink()
        with socket.socket() as listener:
            listener.bind(('127.0.0.1',0)); port = listener.getsockname()[1]
        token = 'local-service-' + uuid.uuid4().hex
        users = json.dumps([{'id':'alice','token_sha256':hashlib.sha256(token.encode()).hexdigest()}])
        name = 'agenct-smoke-' + uuid.uuid4().hex
        def request(path, body=None):
            req = urllib.request.Request(f'http://127.0.0.1:{port}'+path,
                data=None if body is None else json.dumps(body).encode(),
                headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
            with urllib.request.urlopen(req,timeout=2) as response: return json.load(response)
        def ready():
            for _ in range(100):
                try:
                    if request('/api/me')['user']=='alice': return
                except OSError: pass
                time.sleep(.1)
            raise RuntimeError('user service failed to become ready')
        try:
            run('systemd-run','--user','--collect','--unit='+name,
                '--property=Restart=on-failure','--property=RestartSec=0.3','--property=UMask=0077',
                '--property=WorkingDirectory='+str(ROOT),
                '--setenv=AGENCT_BIND=127.0.0.1:'+str(port),
                '--setenv=AGENCT_DATABASE='+str(folder/'hub.sqlite3'),
                '--setenv=AGENCT_RUNTIME=mock','--setenv=AGENCT_USERS_JSON='+users,
                str(ROOT/'hub/target/debug/agenct-hub'))
            ready()
            request('/api/messages',{'message':'Local service persistence fixture','request_id':'service-smoke'})
            previous = run('systemctl','--user','show',name,'--property=MainPID','--value')
            run('systemctl','--user','kill','--signal=SIGKILL','--kill-who=main',name)
            for _ in range(100):
                restarts = run('systemctl','--user','show',name,'--property=NRestarts','--value')
                current = run('systemctl','--user','show',name,'--property=MainPID','--value')
                if int(restarts or 0)>=1 and current not in ('0',previous): break
                time.sleep(.1)
            else: raise RuntimeError('systemd did not restart the crashed Hub')
            ready()
            assert len(request('/api/messages'))>=2
        finally:
            subprocess.run(['systemctl','--user','stop',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    print('PASS real local user-systemd crash auto-restart and database persistence; enable symlink verified in isolated root')
    print('LIMIT: no host reboot, no system-wide service installation')


if __name__=='__main__': main()
