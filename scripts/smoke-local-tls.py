#!/usr/bin/env python3
"""Real local Caddy TLS -> Hub smoke; ephemeral CA trust, no external host or provider."""
import argparse
import hashlib
import http.client
import json
import os
from pathlib import Path
import signal
import socket
import ssl
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def port():
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        return listener.getsockname()[1]


def stop(process):
    if process.poll() is None:
        process.send_signal(signal.SIGINT)
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired: process.kill(); process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--caddy', default='caddy')
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='agenct-local-tls-') as temporary:
        state = Path(temporary)
        upstream, ingress = port(), port()
        while ingress == upstream: ingress = port()
        token = 'local-tls-fixture-' + 'x'*40
        hub_env = {'PATH': os.environ['PATH'], 'AGENCT_BIND':f'127.0.0.1:{upstream}',
                   'AGENCT_DATABASE':str(state/'hub.sqlite3'), 'AGENCT_RUNTIME':'mock',
                   'AGENCT_USERS_JSON':json.dumps([{'id':'alice','token_sha256':hashlib.sha256(token.encode()).hexdigest()}])}
        config = state/'Caddyfile'
        config.write_text((ROOT/'deploy/Caddyfile.local.example').read_text().replace(':8443',f':{ingress}').replace(':8080',f':{upstream}'))
        env = {'PATH':os.environ['PATH'], 'XDG_DATA_HOME':str(state/'data'), 'XDG_CONFIG_HOME':str(state/'config')}
        with (state/'events.log').open('w') as log:
            hub = subprocess.Popen([str(ROOT/'hub/target/debug/agenct-hub')], cwd=ROOT, env=hub_env, stdout=log, stderr=log)
            caddy = None
            try:
                caddy = subprocess.Popen([args.caddy,'run','--config',str(config),'--adapter','caddyfile'], env=env, stdout=log, stderr=log)
                ca = state/'data/caddy/pki/authorities/local/root.crt'
                for _ in range(100):
                    if hub.poll() is not None or caddy.poll() is not None: raise RuntimeError('local service exited')
                    if ca.exists():
                        try:
                            with socket.create_connection(('127.0.0.1',ingress),timeout=.2): break
                        except OSError: pass
                    time.sleep(.1)
                else: raise RuntimeError('local TLS ingress did not start')
                def request(context, path, authenticated=False):
                    client = http.client.HTTPSConnection('agenct.localhost',ingress,context=context,timeout=5)
                    client._create_connection = lambda address, timeout, source_address: socket.create_connection(('127.0.0.1',ingress),timeout)
                    try:
                        client.request('GET',path,headers={'Authorization':'Bearer '+token} if authenticated else {})
                        response=client.getresponse(); return response.status,json.loads(response.read())
                    finally: client.close()
                trusted=ssl.create_default_context(cafile=str(ca))
                for _ in range(100):
                    try:
                        if request(trusted,'/health')[0]==200: break
                    except (OSError, ValueError): pass
                    time.sleep(.1)
                else: raise RuntimeError('verified local TLS request did not become ready')
                try: request(ssl.create_default_context(),'/health')
                except ssl.SSLCertVerificationError: pass
                else: raise RuntimeError('ephemeral local CA unexpectedly trusted globally')
                assert request(trusted,'/api/me')[0]==401
                assert request(trusted,'/api/me',True)[1]['user']=='alice'
            finally:
                if caddy: stop(caddy)
                stop(hub)
        assert token not in (state/'events.log').read_text()
    print('PASS real local Caddy TLS -> Hub: explicit CA trust, hostname verification, authentication, no public listener')
    print('LIMIT: local TLS only; real IdP and production deployment not claimed')


if __name__=='__main__': main()
