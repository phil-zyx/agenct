#!/usr/bin/env python3
"""Run the prepared loopback Hub. HTTP localhost needs no proxy or certificate."""
import argparse
import json
import importlib.util
import os
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--state-dir', type=Path, required=True)
parser.add_argument('--model-state-dir', type=Path, help='Reuse a prepared DSH virtualenv; provider key stays in the process environment')
args = parser.parse_args()
config = json.loads((args.state_dir / 'launch.json').read_text())
# Do not accidentally inherit developer auth or a different provider/tenant configuration.
env = {k:v for k,v in os.environ.items() if not k.startswith('AGENCT_')}
env.update(config)
# The identity provider may only be reachable through the workstation proxy;
# the Hub ignores ambient proxy variables unless one is named for this purpose.
ambient = os.environ.get('HTTPS_PROXY') or os.environ.get('https_proxy')
if ambient and not env.get('AGENCT_IDENTITY_PROXY'):
    env['AGENCT_IDENTITY_PROXY'] = ambient
if env.get('AGENCT_IDENTITY_PROXY'):
    print('GitLab token/userinfo requests use the identity proxy from HTTPS_PROXY / AGENCT_IDENTITY_PROXY', flush=True)
binary = ROOT / 'hub/target/release/agenct-hub'
if not binary.is_file(): raise SystemExit('Build first: cargo build --locked --release --manifest-path hub/Cargo.toml')
os.umask(0o077)
os.chdir(ROOT)
if args.model_state_dir:
    spec = importlib.util.spec_from_file_location('private_staging', ROOT / 'scripts/private-staging.py')
    staging = importlib.util.module_from_spec(spec); spec.loader.exec_module(staging)
    env['AGENCT_RUNTIME'] = 'dsh'
    staging.supervise_dsh(args.model_state_dir, env)
else:
    os.execve(binary, [str(binary)], env)
