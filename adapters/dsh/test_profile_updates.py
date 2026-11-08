"""Offline conversational settings protocol: proposals never grant permissions."""
import json
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import Mock
import uuid

from adapter import SUMMARY_MESSAGE, parse_model_response, run_sdk, validate_turn
from test_adapter import turn
from test_draft_delivery import draft
from test_owner_context import owner_context


def profile_context():
    return {"profile": {"revision": 0, **owner_context(), "permissions": {"auto_reply_public": False, "auto_prepare_drafts": True}}, "pending": None}


class ProfileUpdateTests(unittest.TestCase):
    def test_legacy_reply_and_sparse_candidate_are_supported(self):
        old = parse_model_response('{"reply":"hello","proposal":null}', 'hello')
        self.assertEqual(set(old), {'reply', 'proposal', 'backend'})
        for changes in ({"private": {"work_context": "我负责支付服务"}},
                        {"public": {"team": "研发"}, "permissions": {"auto_reply_public": True}},
                        {"private": {"response_preferences": ""}}, {"permissions": {"auto_prepare_drafts": False}}):
            with self.subTest(changes=changes):
                parsed = parse_model_response(json.dumps({"reply": "请确认", "proposal": None, "profile_update": changes}), '记住我的偏好')
                self.assertEqual(parsed['profile_update'], changes)

    def test_invalid_or_overpowered_changes_are_rejected(self):
        invalid = [True, [], {}, {"private": {}}, {"owner": "bob"}, {"tools": ["shell"]},
                   {"private": {"work_context": None}}, {"private": {"response_preferences": "x" * 1001}},
                   {"public": {"team": "x" * 121}}, {"public": {"token": "secret"}},
                   {"permissions": {"auto_reply_public": "true"}}, {"permissions": {"auto_prepare_drafts": 1}},
                   {"permissions": {"auto_deliver": True}}, {"private": {"work_context": "\x00"}}]
        for changes in invalid:
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                parse_model_response(json.dumps({"reply": "please confirm", "proposal": None, "profile_update": changes}), 'hello')
        with self.assertRaisesRegex(ValueError, 'delegation_forbidden'):
            parse_model_response(json.dumps({"reply": "ok", "proposal": {"owner": "bob", "summary": "review"},
                                             "profile_update": {"private": {"work_context": "review"}}}), 'ask bob to review')

    def test_profile_context_and_pending_are_strict_chat_only_data(self):
        value = profile_context()
        value['pending'] = {"id": str(uuid.uuid4()), "expected_revision": 0, "changes": {"private": {"response_preferences": "短一些"}},
                            "status": "pending", "created_at": "2026-11-07 12:00:00"}
        validate_turn(turn() | {'profile_context': value})
        invalid = [None, {}, value | {'owner': 'bob'}, value | {'profile': value['profile'] | {'revision': True}},
                   value | {'profile': value['profile'] | {'permissions': {'auto_reply_public': True}}},
                   value | {'pending': value['pending'] | {'status': 'confirmed'}},
                   value | {'pending': value['pending'] | {'changes': {'permissions': {'tools': True}}}}]
        factory = Mock()
        for context in invalid:
            with self.subTest(context=context), self.assertRaises(ValueError):
                run_sdk(turn() | {'profile_context': context}, Path('/unused'), Path('/unused'), factory)
        factory.assert_not_called()
        summary = turn() | {'kind': 'context_summary', 'message': SUMMARY_MESSAGE, 'history': [{'role': 'user', 'content': 'shared document'}]}
        for request in (draft(), summary):
            with self.assertRaisesRegex(ValueError, 'profile_context_chat_only'):
                validate_turn(request | {'profile_context': profile_context()})
        for kind in ('draft_delivery', 'context_summary'):
            with self.assertRaisesRegex(ValueError, 'profile_update_chat_only'):
                parse_model_response('{"reply":"ok","proposal":null,"profile_update":null}', 'hello', kind)

    def test_sdk_gets_untrusted_profile_data_without_history_mutation(self):
        captured = []
        changes = {'private': {'response_preferences': '先写结论'}}
        class Fake:
            def __init__(self, **kwargs): pass
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def run(self, prompt, session_id):
                captured.append(prompt)
                return SimpleNamespace(finish_reason='completed', final_response=json.dumps({'reply': '请确认', 'proposal': None, 'profile_update': changes}))
        request = turn() | {'message': '记住：回复先写结论', 'profile_context': profile_context()}
        result = run_sdk(request, Path('/unused'), Path('/unused'), Fake)
        self.assertEqual(result['profile_update'], changes)
        rules, raw = captured[0].split('\n', 1)
        self.assertIn('CURRENT USER MESSAGE explicitly', rules)
        self.assertIn('Only put data in public.', rules)
        self.assertIn('Only change permissions.', rules)
        self.assertIn('never claim a profile or permission was saved', rules)
        payload = json.loads(raw)
        self.assertEqual(payload['profile_context'], request['profile_context'])
        self.assertEqual(payload['history'], request['history'])


if __name__ == '__main__':
    unittest.main()
