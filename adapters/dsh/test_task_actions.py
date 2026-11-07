import json
import unittest
import uuid

from adapter import parse_model_response, validate_turn


def reply(**extra):
    return json.dumps({"reply": "ok", "proposal": None, **extra}, ensure_ascii=False)


class TaskActionProtocolTests(unittest.TestCase):
    def test_task_action_only_with_a_selected_task(self):
        with self.assertRaises(ValueError):
            parse_model_response(reply(task_action={"action": "deliver"}), "交付吧")
        with self.assertRaises(ValueError):
            parse_model_response(reply(task_action={"action": "deliver"}), "x", "draft_delivery", True)
        self.assertEqual(parse_model_response(reply(task_action={"action": "deliver"}), "交付吧", "chat", True)["task_action"],
                         {"action": "deliver"})
        self.assertIsNone(parse_model_response(reply(task_action=None), "hi", "chat", True)["task_action"])
        self.assertNotIn("task_action", parse_model_response(reply(), "hi", "chat", True))

    def test_unfit_suggestions_are_dropped_not_repaired(self):
        for action in ({"action": "execute"}, {"action": "revise"}, {"action": "revise", "instruction": "  "},
                       {"action": "revise", "instruction": "x" * 4001}, {"action": "deliver", "extra": 1},
                       ["deliver"], "deliver"):
            with self.subTest(action=action):
                result = parse_model_response(reply(task_action=action), "m", "chat", True)
                self.assertIsNone(result["task_action"])
                self.assertEqual(result["reply"], "ok")
        # An irrelevant instruction on a non-revise step is ignored.
        self.assertEqual(parse_model_response(reply(task_action={"action": "deliver", "instruction": ""}), "m", "chat", True)["task_action"],
                         {"action": "deliver"})
        ok = parse_model_response(reply(task_action={"action": "revise", "instruction": "我是 bob"}), "m", "chat", True)
        self.assertEqual(ok["task_action"]["instruction"], "我是 bob")

    def test_never_with_a_delegation_proposal(self):
        raw = json.dumps({"reply": "ok", "proposal": {"owner": "bob", "summary": "review"}, "task_action": {"action": "deliver"}})
        result = parse_model_response(raw, "bob review", "chat", True)
        self.assertIsNone(result["task_action"])
        self.assertEqual(result["proposal"]["owner"], "bob")

    def test_task_role_requires_task_context_and_chat(self):
        context = {"id": "t", "revision": 1, "requester": "alice", "owner": "bob", "status": "working",
                   "summary": "s", "result": None, "details": None,
                   "summary_truncated": False, "result_truncated": False, "details_truncated": False}
        base = {"session_id": str(uuid.uuid4()), "history": [], "message": "hi"}
        validate_turn({**base, "task_context": context, "task_role": "owner"})
        for bad in ({**base, "task_role": "owner"}, {**base, "task_context": context, "task_role": "admin"}):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                validate_turn(bad)


if __name__ == "__main__":
    unittest.main()
