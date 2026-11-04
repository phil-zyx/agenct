#!/usr/bin/python3
"""Disabled entrypoint: no code execution or successful evidence is possible.

Validation helpers support offline protocol tests only. Re-enable only after
independently trusted verification and adversarial security review.
"""
import base64
import re
import sys


def validate(data):
    if set(data) != {'repository', 'base_commit', 'bundle_base64', 'patch', 'checks'}:
        raise ValueError('unknown or missing fields')
    if not re.fullmatch(r'[a-zA-Z0-9_.-]{1,128}', data['repository']):
        raise ValueError('repository key')
    if not re.fullmatch(r'(?:[a-f0-9]{40}|[a-f0-9]{64})', data['base_commit']):
        raise ValueError('base commit')
    if not isinstance(data['patch'], str) or not 0 < len(data['patch'].encode()) <= 1024 * 1024:
        raise ValueError('patch size')
    if not isinstance(data['bundle_base64'], str) or len(data['bundle_base64']) > 12 * 1024 * 1024:
        raise ValueError('bundle size')
    bundle = base64.b64decode(data['bundle_base64'], validate=True)
    if not 0 < len(bundle) <= 8 * 1024 * 1024:
        raise ValueError('decoded bundle size')
    checks = data['checks']
    if not isinstance(checks, list) or not 0 < len(checks) <= 16:
        raise ValueError('checks')
    names = set()
    for check in checks:
        if set(check) != {'name', 'argv'} or not re.fullmatch(r'[a-zA-Z0-9_.-]{1,128}', check['name']):
            raise ValueError('check schema')
        if check['name'] in names:
            raise ValueError('duplicate check')
        names.add(check['name'])
        if not isinstance(check['argv'], list) or not 0 < len(check['argv']) <= 32:
            raise ValueError('check argv')
        if any(not isinstance(arg, str) or not 0 < len(arg) <= 1024 or '\0' in arg for arg in check['argv']):
            raise ValueError('check argv item')
    return bundle



def main():
    # Deliberately no clone, patch application, test process or success evidence.
    # Repository tests previously shared this runner's UID and could rewrite the
    # index/replace refs. A post-test git diff cannot establish tested tree identity.
    raise RuntimeError('execution disabled pending independent trusted verification')


if __name__ == '__main__':
    try:
        main()
    except Exception:
        print('isolated worker disabled: trusted verification unavailable', file=sys.stderr)
        raise SystemExit(1)
