#!/usr/bin/env python3
"""Fixed read-only Git-object context helper. Never runs Git, hooks, shell or repo code."""
import json
import os
from pathlib import Path
import resource
import sys

if "independent_verifier" not in sys.modules:
    sys.path.insert(0, str(Path(__file__).resolve().parent))
from independent_verifier import LooseObjectStore, Rejected, extract_snapshot

MAX_RESPONSE = 12000
MAX_TEXT = 4000


def context(request):
    if not isinstance(request, dict) or set(request) - {'objects_dir', 'commit', 'path', 'query'}:
        raise Rejected('invalid request')
    objects = request.get('objects_dir')
    if not isinstance(objects, str) or not Path(objects).is_absolute():
        raise Rejected('invalid object directory')
    commit = request.get('commit')
    if not isinstance(commit, str):
        raise Rejected('full commit required')
    path, query = request.get('path'), request.get('query')
    if (path is None) == (query is None):
        raise Rejected('choose a path or literal search')
    if path is not None and (not isinstance(path, str) or not path or len(path.encode()) > 1024
                             or any(part in ('', '.', '..', '.git') for part in path.split('/'))
                             or any(ord(c) < 32 or c == '\\' for c in path)):
        raise Rejected('invalid file path')
    if query is not None and (not isinstance(query, str) or not query.strip() or len(query.encode()) > 200
                              or any(ord(c) < 32 for c in query)):
        raise Rejected('invalid literal query')
    snapshot = extract_snapshot(LooseObjectStore(objects), commit)
    text, matches, truncated = [], 0, False
    used = 0
    skipped_files = 0
    found = False
    for file in snapshot.files:
        if path is not None and file.path != path:
            continue
        found = True
        if len(file.content) > 262144 or b'\0' in file.content:
            if path is not None:
                raise Rejected('file is binary or exceeds 256 KiB')
            skipped_files += 1
            truncated = True
            continue
        try:
            content = file.content.decode('utf-8')
        except UnicodeError:
            if path is not None:
                raise Rejected('UTF-8 text files only')
            skipped_files += 1
            truncated = True
            continue
        if any(ord(ch) < 32 and ch not in '\n\r\t' for ch in content):
            if path is not None:
                raise Rejected('control characters forbidden')
            skipped_files += 1
            truncated = True
            continue
        for number, line in enumerate(content.splitlines(), 1):
            if query is not None and query not in line:
                continue
            excerpt = f'{file.path}:{number}: {line}'
            size = len(excerpt.encode()) + 1
            if used + size > MAX_TEXT or matches >= 40:
                truncated = True
                # Skip oversized lines, never cut through UTF-8 or pretend complete.
                continue
            text.append(excerpt)
            used += size
            matches += 1
    if not found and path is not None:
        raise Rejected('file not present in selected commit')
    if truncated and not text and not skipped_files:
        raise Rejected('selected lines exceed context budget; use a narrower file or literal query')
    result = {'commit': snapshot.commit, 'snapshot_digest': snapshot.digest,
              'path': path, 'query': query, 'text': '\n'.join(text), 'truncated': truncated,
              'matches': matches, 'skipped_files': skipped_files, 'read_only': True}
    if len(json.dumps(result, ensure_ascii=False).encode()) > MAX_RESPONSE:
        raise Rejected('response budget exceeded')
    return result


def main():
    # This process is an object parser, not a code executor. Enforce bounds before
    # processing any repository-controlled compressed bytes.
    resource.setrlimit(resource.RLIMIT_CPU, (3, 3))
    resource.setrlimit(resource.RLIMIT_AS, (256 * 1024 * 1024, 256 * 1024 * 1024))
    resource.setrlimit(resource.RLIMIT_NOFILE, (32, 32))
    raw = sys.stdin.buffer.read(8193)
    if len(raw) > 8192:
        raise Rejected('request budget exceeded')
    value = context(json.loads(raw))
    print(json.dumps(value, ensure_ascii=False))


if __name__ == '__main__':
    try:
        main()
    except Exception:
        # Never disclose source paths, content or decompressor diagnostics in errors.
        print('{"error":"repository_context_unavailable"}')
        sys.exit(1)
