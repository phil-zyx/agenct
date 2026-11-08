#!/usr/bin/env python3
"""Prepare Agenct GitLab browser login from an existing OAuth application.
Never changes the GitLab application, copies private chats, or infers account ownership.
"""
import argparse
import hashlib
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import urllib.parse

def read_env(path):
    values = {}
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith('#') or '=' not in line: continue
        key, value = line.removeprefix('export ').split('=', 1)
        values[key.strip()] = value.strip().strip('"').strip("'")
    return values

def secure_write(path, text):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, 'w') as out: out.write(text)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source-env', required=True, type=Path)
    parser.add_argument('--auto-signup', action='store_true', help='Create fresh gitlab-ID accounts for active users of this GitLab instance')
    parser.add_argument('--subjects', required=False, type=Path, help='Explicit GitLab numeric user ID -> local account mapping JSON')
    parser.add_argument('--admin', required=False, action='append', default=[], help='Local account ID; repeat for multiple admins')
    parser.add_argument('--state-dir', required=True, type=Path)
    parser.add_argument('--reuse-registered-callback', action='store_true', help='Run Agenct at the existing callback origin and path')
    parser.add_argument('--shared-login-url', help='Tidalis GitLab login endpoint; reuse its registered callback')
    parser.add_argument('--public-origin')
    parser.add_argument('--port', type=int)
    parser.add_argument('--organization', default='qivators')
    args = parser.parse_args()
    args.public_origin = args.public_origin or ('http://localhost:8081' if args.shared_login_url else 'https://localhost:8443')
    args.port = args.port or (8081 if args.shared_login_url else 8082)
    source = read_env(args.source_env)
    callback = source.get('GITLAB_OAUTH_REDIRECT_URI', '')
    if args.reuse_registered_callback:
        if args.shared_login_url: raise ValueError('direct callback and shared login are mutually exclusive')
        registered = urllib.parse.urlsplit(callback)
        if registered.path not in ('/auth/callback', '/identity/v1/auth/oidc/gitlab/callback') or registered.query or registered.fragment or registered.username or registered.password:
            raise ValueError('unsupported registered callback')
        args.public_origin = registered.scheme + '://' + registered.netloc
        args.port = registered.port or (443 if registered.scheme == 'https' else 80)
    issuer = source.get('GITLAB_OAUTH_BASE_URL', 'https://git.qivators.com').rstrip('/')
    origin = urllib.parse.urlsplit(issuer)
    public = urllib.parse.urlsplit(args.public_origin)
    if origin.scheme != 'https' or origin.username or origin.password or origin.query or origin.fragment or origin.path:
        raise ValueError('GitLab issuer must be an HTTPS origin')
    local_http = public.scheme == 'http' and public.hostname in ('localhost', '127.0.0.1', '::1')
    private_http = False
    if args.reuse_registered_callback and public.scheme == 'http':
        try:
            ip = ipaddress.ip_address(public.hostname)
            private_http = any(ip in ipaddress.ip_network(net) for net in ('10.0.0.0/8','172.16.0.0/12','192.168.0.0/16','100.64.0.0/10'))
        except ValueError: pass
    local_http = local_http or private_http
    if (public.scheme != 'https' and not local_http) or public.username or public.password or public.query or public.fragment or public.path:
        raise ValueError('public-origin must be an exact HTTPS or loopback HTTP origin')
    callback = source.get('GITLAB_OAUTH_REDIRECT_URI', '')
    if args.shared_login_url:
        shared = urllib.parse.urlsplit(args.shared_login_url)
        if (shared.scheme != 'https' and not (shared.scheme == 'http' and shared.hostname in ('localhost', '127.0.0.1', '::1'))) or shared.username or shared.password or shared.query or shared.fragment or not callback:
            raise ValueError('shared login requires a trusted HTTPS/loopback endpoint and source callback')
    if not 1024 <= args.port <= 65535: raise ValueError('invalid local port')
    client = source.get('GITLAB_OAUTH_CLIENT_ID', '')
    secret = source.get('GITLAB_OAUTH_CLIENT_SECRET', '')
    if not client or not secret: raise ValueError('source environment lacks OAuth client credentials')
    if args.auto_signup and (args.subjects or args.admin): raise ValueError('automatic signup starts without legacy mappings or inferred administrators')
    if not args.auto_signup and not args.subjects: raise ValueError('choose --auto-signup or --subjects')
    subjects = {}; ids = set()
    if not args.auto_signup:
        pairs = json.loads(args.subjects.read_text(), object_pairs_hook=list)
        if not isinstance(pairs, list) or not pairs: raise ValueError('subjects must be a nonempty object')
        subjects = {}
        ids = set()
        for sub, user in pairs:
            if not isinstance(sub, str) or not re.fullmatch(r'[1-9][0-9]{0,19}', sub) or int(sub) > 2**64-1 or sub in subjects or not isinstance(user, str) or not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', user) or user in ids:
                raise ValueError('each unique subject must bind exactly one unique valid local account')
            subjects[sub] = user; ids.add(user)
    if not set(args.admin) <= ids: raise ValueError('administrators must be mapped local accounts')
    if not re.fullmatch(r'[A-Za-z0-9_-]{1,64}', args.organization): raise ValueError('invalid organization')
    state = args.state_dir.resolve()
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    if state.is_symlink() or state.stat().st_mode & 0o077: raise ValueError('state directory must be private (chmod 700)')
    outputs = ['browser-oidc.json','client-secret','tenants.json','launch.json','Caddyfile']
    if any((state / name).exists() for name in outputs): raise ValueError('configuration already exists; use a new private directory')
    config = {'gitlab_auto_signup':args.auto_signup,'gitlab_oauth':True,'issuer':issuer,'authorization_endpoint':issuer+'/oauth/authorize','token_endpoint':issuer+'/oauth/token', 'client_id':client,'public_origin':args.public_origin,'subjects':subjects,'jwks':{'keys':[]}}
    config['allow_loopback_http'] = local_http
    if args.reuse_registered_callback:
        config.update(callback_path=registered.path, allow_private_http=private_http)
    if args.shared_login_url:
        config.update(shared_gitlab_login=args.shared_login_url, shared_gitlab_callback=callback)
    secure_write(state/'browser-oidc.json', json.dumps(config, indent=2))
    secure_write(state/'client-secret', secret)
    secure_write(state/'tenants.json', json.dumps({user:args.organization for user in sorted(ids)}, indent=2))
    env = {'AGENCT_BIND':f'127.0.0.1:{args.port}','AGENCT_DATABASE':str(state/'hub.sqlite3'),'AGENCT_RUNTIME':'mock',
           'AGENCT_USERS_JSON':json.dumps([{'id':user,'token_sha256':hashlib.sha256(secrets.token_bytes(32)).hexdigest()} for user in (sorted(ids) if ids else ['login-bootstrap'])]),
           'AGENCT_BROWSER_OIDC_CONFIG_FILE':str(state/'browser-oidc.json'),'AGENCT_OIDC_CLIENT_SECRET_FILE':str(state/'client-secret'),
           'AGENCT_TENANTS_FILE':str(state/'tenants.json'),'AGENCT_MEMBER_ADMINS_JSON':json.dumps(args.admin)}
    if args.reuse_registered_callback and private_http:
        env['AGENCT_BIND'] = f'{public.hostname}:{args.port}'
        env['AGENCT_PRIVATE_BIND_IP'] = public.hostname
    if args.auto_signup:
        env.pop('AGENCT_TENANTS_FILE', None); env.pop('AGENCT_MEMBER_ADMINS_JSON', None)
    secure_write(state/'launch.json',json.dumps(env,indent=2))
    if not local_http: secure_write(state/'Caddyfile', '{\n admin off\n auto_https disable_redirects\n skip_install_trust\n}\n' + args.public_origin + ' {\n bind 127.0.0.1\n tls internal\n reverse_proxy 127.0.0.1:' + str(args.port) + '\n}\n')
    print('Prepared private configuration in', state)
    if args.reuse_registered_callback:
        print('Start Agenct at:', args.public_origin, '; existing callback is handled directly.')
    elif args.shared_login_url:
        print('Add this origin to Identity OIDC allowed origins:', args.public_origin)
        print('Reuse the existing registered GitLab callback; no additional callback needed.')
    else:
        print('Register this additional callback without removing existing ones:', args.public_origin + '/auth/callback')
    print('Scope: read_user. No credentials were printed. New isolated database; existing Alice/Bob data is untouched.')

if __name__ == '__main__':
    main()
