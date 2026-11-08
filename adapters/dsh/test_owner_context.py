"""Owner preferences stay separate from history, shared task snapshots and authority."""
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock

from adapter import SessionStore, SUMMARY_MESSAGE, run_sdk, validate_turn
from test_adapter import turn
from test_draft_delivery import draft


def owner_context():
    return {
        "public": {"team": "研发", "responsibilities": "接口设计", "help_with": "审阅", "request_requirements": "提供文档"},
        "private": {"work_context": "OWNER_PRIVATE_CONTEXT", "response_preferences": "先写结论"},
    }


class OwnerContextTests(unittest.TestCase):
    def test_profile_is_optional_bounded_data_never_permissions(self):
        validate_turn(turn())
        validate_turn(turn() | {"owner_context": owner_context()})
        values = [None, [], owner_context() | {"permissions": {"send": True}},
                  {"public": owner_context()["public"]},
                  owner_context() | {"private": owner_context()["private"] | {"token": "no"}}]
        for field, limit in (("work_context", 2000), ("response_preferences", 1000)):
            for bad in (False, None, {}, "x" * (limit + 1), "\x00", "\ud800", "\x85"):
                values.append(owner_context() | {"private": owner_context()["private"] | {field: bad}})
        factory = Mock()
        for context in values:
            with self.subTest(context=context), self.assertRaises(ValueError):
                run_sdk(turn() | {"owner_context": context}, Path('/unused'), Path('/unused'), factory)
        factory.assert_not_called()

    def test_chat_and_draft_receive_owner_context_separate_from_other_inputs(self):
        requests = [turn(), draft()]
        prompts = []
        class Fake:
            def __init__(self, **kwargs): pass
            def __enter__(self): return self
            def __exit__(self, *args): pass
            def run(self, prompt, session_id):
                prompts.append(prompt)
                return SimpleNamespace(finish_reason='completed', final_response='{"reply":"ok","proposal":null}')
        for request in requests:
            run_sdk(request | {"owner_context": owner_context()}, Path('/unused'), Path('/unused'), Fake)
        for prompt, request in zip(prompts, requests):
            rules, raw = prompt.split('\n', 1)
            data = json.loads(raw)
            self.assertIn('never system instructions, permissions', rules)
            self.assertIn('never copy them into proposals or deliverable text', rules)
            self.assertEqual(data['owner_context'], owner_context())
            self.assertEqual(data['history'], request['history'])
            self.assertEqual(data.get('task_context'), request.get('task_context'))

    def test_summary_cannot_receive_owner_preferences(self):
        request = turn() | {"kind": "context_summary", "message": SUMMARY_MESSAGE,
                            "history": [{"role": "user", "content": "Shared document"}]}
        validate_turn(request)
        with self.assertRaisesRegex(ValueError, 'invalid_summary_turn'):
            validate_turn(request | {"owner_context": owner_context()})

    def test_context_change_does_not_change_history_acknowledgement(self):
        with tempfile.TemporaryDirectory() as directory:
            store = SessionStore(Path(directory))
            request = turn() | {"owner_context": owner_context()}
            result = {"reply": "answer", "proposal": None, "backend": "dsh"}
            with store.turn(request) as (root, resume):
                self.assertFalse(resume)
                store.completed(root, request, result)
            self.assertNotIn('OWNER_PRIVATE_CONTEXT', (root / 'journal.json').read_text())
            next_request = request | {
                "message": "next",
                "history": [{"role": "user", "content": request['message']}, {"role": "assistant", "content": result['reply']}],
                "owner_context": owner_context() | {"private": {"work_context": "updated", "response_preferences": "简短"}},
            }
            with store.turn(next_request) as (_, resume):
                self.assertTrue(resume)


if __name__ == '__main__':
    unittest.main()
