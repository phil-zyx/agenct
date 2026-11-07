#!/usr/bin/env python3
"""Real Orca/AT-SPI speech-generation smoke. Requires orca, xvfb, xauth, dbus-run-session.
No audio hardware is required; validates Orca's generated utterances, not a human listening test.
"""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--full', action='store_true', help='also traverse real local Hub delegation/draft/delivery')
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    output = ROOT/'test-results'/'screen-reader'
    output.mkdir(parents=True, exist_ok=True)
    if not args.inside:
        for name in ('orca', 'xvfb-run', 'xauth', 'dbus-run-session'):
            if not shutil.which(name): parser.error(f'{name} missing; install orca xvfb xauth')
        return subprocess.call(['xvfb-run', '-a', 'dbus-run-session', '--', sys.executable, __file__, '--inside'] + (['--full'] if args.full else []), cwd=ROOT)
    log = output/'orca.log'
    log.unlink(missing_ok=True)
    env = os.environ.copy()
    env.update(GSETTINGS_BACKEND='memory', NO_AT_BRIDGE='0')
    with (output/'process.log').open('w') as diagnostics:
        process = subprocess.Popen(['orca', '--enable=speech', '--disable=braille', '--user-prefs='+str(output/'prefs'), '--debug-file='+str(log)], env=env, stdout=diagnostics, stderr=diagnostics)
        try:
            subprocess.run(['node', 'scripts/screen-reader-smoke.cjs'], cwd=ROOT, env=env, check=True, timeout=90)
            if args.full:
                env['AGENCT_TEST_HEADED'] = '1'
                subprocess.run(['node', 'scripts/local-browser-acceptance.cjs'], cwd=ROOT, env=env, check=True, timeout=180)
        finally:
            process.terminate()
            try: process.wait(timeout=10)
            except subprocess.TimeoutExpired: process.kill(); process.wait()
    utterances = '\n'.join(line for line in log.read_text().splitlines() if 'SPEECH OUTPUT:' in line)
    for expected in ('进入', '1 / 3', '给管家发消息', '发送', '工作'):
        if expected not in utterances: raise RuntimeError('Orca did not announce expected control: '+expected)
    if args.full:
        for expected in ('接手', '草稿修改要求', '确认交付', '验收'):
            if expected not in utterances: raise RuntimeError('Orca did not announce workflow control: '+expected)
        print('PASS real Orca announced accept, draft revision, delivery and approval controls with real Hub')
    print('PASS real Orca announced login, onboarding, composer, send and work navigation')
    print('LIMIT: generated speech inspected; no human auditory usability claim')
    return 0


if __name__ == '__main__':
    sys.exit(main())
