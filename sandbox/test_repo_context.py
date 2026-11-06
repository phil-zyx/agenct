"""Read-only repository context protocol: real loose objects, no Git or hooks."""
import json
from pathlib import Path
import tempfile
import unittest
import zlib

import independent_verifier as verifier
from repo_context import context


class RepoContextTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.objects = Path(self.temp.name) / 'objects'
        self.objects.mkdir()

    def object(self, kind, body):
        oid = verifier.digest_object(kind, body, 40)
        path = self.objects / oid[:2] / oid[2:]
        path.parent.mkdir(exist_ok=True)
        path.write_bytes(zlib.compress(kind.encode() + b' ' + str(len(body)).encode() + b'\0' + body))
        return oid

    def request(self, files, **selection):
        tree = b''
        for name, mode, data in files:
            oid = self.object('blob', data)
            tree += mode.encode() + b' ' + name.encode() + b'\0' + bytes.fromhex(oid)
        oid = self.object('tree', tree)
        commit = self.object('commit', b'tree ' + oid.encode() + b'\n\nfixture\n')
        return {'objects_dir': str(self.objects), 'commit': commit, **selection}

    def test_reads_exact_commit_and_literal_search_without_running_source(self):
        marker = Path(self.temp.name) / 'must-not-exist'
        script = ('#!/bin/sh\ntouch ' + str(marker) + '\n').encode()
        request = self.request([('source.txt', '100644', 'First line\n需求：blue.* literal\nLast line\n'.encode()),
                                ('hook.sh', '100755', script)], path='source.txt')
        value = context(request)
        self.assertEqual(value['commit'], request['commit'])
        self.assertIn('source.txt:2: 需求：blue.* literal', value['text'])
        self.assertFalse(value['truncated'])
        search = context(request | {'path': None, 'query': 'blue.*'})
        self.assertEqual(search['matches'], 1)
        self.assertNotIn('First line', search['text'])
        self.assertFalse(marker.exists())
        self.assertEqual(len(value['snapshot_digest']), 64)

    def test_rejects_paths_special_files_binary_size_and_corrupt_objects(self):
        request = self.request([('source.txt', '100644', b'plain')], path='source.txt')
        for change in ({'path': '../source.txt'}, {'path': '/source.txt'}, {'path': '.git/config'},
                       {'path': 'missing'}, {'commit': 'HEAD'}, {'query': 'also'}, {'path': None}):
            with self.subTest(change=change), self.assertRaises(verifier.Rejected):
                context(request | change)
        for mode, content in [('120000', b'/etc/passwd'), ('100644', b'\0binary'),
                              ('100644', b'\xff'), ('100644', b'\x1bcontrol'), ('100644', b'x' * 262145)]:
            with self.subTest(mode=mode), self.assertRaises(verifier.Rejected):
                context(self.request([('source.txt', mode, content)], path='source.txt'))
        (self.objects / request['commit'][:2] / request['commit'][2:]).write_bytes(b'corrupted')
        with self.assertRaises(verifier.Rejected): context(request)

    def test_bounded_result_marks_incomplete_and_search_reaches_tail(self):
        data = ('ordinary line\n' * 1000 + 'TAIL critical decision\n').encode()
        request = self.request([('source.txt', '100644', data)], path='source.txt')
        value = context(request)
        self.assertTrue(value['truncated'])
        self.assertLessEqual(len(value['text'].encode()), 4000)
        search = context(request | {'path': None, 'query': 'TAIL'})
        self.assertIn('1001: TAIL critical decision', search['text'])
        self.assertFalse(search['truncated'])
        self.assertLess(len(json.dumps(search).encode()), 12000)


if __name__ == '__main__': unittest.main()
