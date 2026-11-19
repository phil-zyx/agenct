"""Unit tests for the unified Agenct execution node (node/agenct-node.py)."""
from __future__ import annotations

import base64
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

NODE_PATH = Path(__file__).resolve().parent / "agenct-node.py"
spec = importlib.util.spec_from_file_location("agenct_node", NODE_PATH)
assert spec and spec.loader
node = importlib.util.module_from_spec(spec)
spec.loader.exec_module(node)


class TestUnifiedNode(unittest.TestCase):
    def test_default_sandbox_is_safe_workspace_write(self) -> None:
        args = node.parse_args(["--hub", "https://agenct.example.com"])
        self.assertEqual(args.sandbox, "workspace-write")
        self.assertFalse(args.unsafe_full_access)
        self.assertEqual(args.kind, "pc")

    def test_journal_replays_ready_submissions_and_ignores_uncertain_running(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            j = node.ExecutionJournal(tmp)
            j.write("tsk_1", 1, {"state": "running", "task_id": "tsk_1", "expected_revision": 1})
            j.write(
                "tsk_2",
                2,
                {
                    "state": "ready_to_submit",
                    "task_id": "tsk_2",
                    "expected_revision": 2,
                    "payload": {"status": "completed", "content": "done"},
                },
            )
            pending = j.pending_submissions()
            self.assertEqual(len(pending), 1)
            self.assertEqual(pending[0]["task_id"], "tsk_2")
            j.remove("tsk_2", 2)
            self.assertEqual(j.pending_submissions(), [])

    def test_attachment_materialization_and_evidence_collection(self) -> None:
        raw = base64.b64encode(b"\x89PNG\r\n\x1a\nhello").decode("ascii")
        paths, in_dir = node.materialize_attachments(
            [{"name": "../../escape.png", "mime_type": "image/png", "data_base64": raw}]
        )
        self.assertIsNotNone(in_dir)
        self.assertEqual(len(paths), 1)
        self.assertTrue(os.path.dirname(paths[0]) == in_dir)
        self.assertNotIn("..", os.path.basename(paths[0]))

        with tempfile.TemporaryDirectory() as ev_dir:
            with open(os.path.join(ev_dir, "shot.png"), "wb") as fh:
                fh.write(b"\x89PNG\r\n\x1a\nok")
            with open(os.path.join(ev_dir, "ignored.exe"), "wb") as fh:
                fh.write(b"MZ")
            items = node.collect_evidence(ev_dir)
            self.assertEqual(len(items), 1)
            self.assertEqual(items[0]["name"], "shot.png")
            self.assertEqual(items[0]["mime_type"], "image/png")

    def test_template_execution_succeeds_without_cli(self) -> None:
        with tempfile.TemporaryDirectory() as workdir:
            import threading

            status, content, atts = node.execute_job(
                "template",
                {"task_id": "tsk_1", "title": "Hello", "instruction": "Inspect repo"},
                workdir,
                "workspace-write",
                10,
                threading.Event(),
                lambda _s, _t: None,
            )
            self.assertEqual(status, "completed")
            self.assertIn("Inspect repo", content)
            self.assertEqual(atts, [])

    def test_strip_ansi_removes_escape_codes_and_control_chars(self) -> None:
        raw = "\x1b[0m\r\n\x1b[32;1m> build · deepseek\x1b[0m\n\x1b]0;title\x07结果：ok\x00\n\t缩进"
        self.assertEqual(node.strip_ansi(raw), "\n> build · deepseek\n结果：ok\n\t缩进")

    def test_parse_opencode_json_extracts_final_message_text_only(self) -> None:
        stream = "\n".join([
            '{"type":"step_start","part":{"type":"step-start","messageID":"m1"}}',
            '{"type":"text","part":{"type":"text","messageID":"m1","text":"先查一下 tmux..."}}',
            '{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"title":"tmux ls","input":{"command":"tmux ls"}}}}',
            '{"type":"step_finish","part":{"type":"step-finish","reason":"tool-calls"}}',
            '{"type":"step_start","part":{"type":"step-start","messageID":"m2"}}',
            '{"type":"text","part":{"type":"text","messageID":"m2","text":"\\u001b[0m当前活跃会话：\\n- `agy` (3 windows)\\n- `cc` (5 windows)"}}',
            '{"type":"step_finish","part":{"type":"step-finish","reason":"stop"}}',
        ])
        parsed = node.parse_opencode_json(stream)
        self.assertEqual(parsed, "当前活跃会话：\n- `agy` (3 windows)\n- `cc` (5 windows)")
        self.assertIsNone(node.parse_opencode_json("plain text output"))

    def test_summarize_progress_line_formats_opencode_events(self) -> None:
        ev = '{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"title":"tmux ls","input":{"command":"tmux ls"}}}}'
        self.assertEqual(node.summarize_progress_line("opencode", ev), "bash: tmux ls")
        self.assertEqual(
            node.summarize_progress_line("opencode", '{"type":"step_start","part":{"type":"step-start"}}'),
            "",
        )
        self.assertEqual(node.summarize_progress_line("claude", "\x1b[31mhi\x1b[0m"), "hi")


if __name__ == "__main__":
    unittest.main()
