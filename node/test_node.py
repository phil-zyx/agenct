"""Unit tests for the unified Agenct execution node (node/agenct-node.py)."""
from __future__ import annotations

import base64
import importlib.util
import json
import os
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

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
        self.assertEqual(args.wait, 25)

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
            '{"type":"step_start","sessionID":"ses_abc12345","part":{"type":"step-start","messageID":"m1"}}',
            '{"type":"text","sessionID":"ses_abc12345","part":{"type":"text","messageID":"m1","text":"先查一下 tmux..."}}',
            '{"type":"tool_use","sessionID":"ses_abc12345","part":{"type":"tool","tool":"bash","state":{"title":"tmux ls","input":{"command":"tmux ls"}}}}',
            '{"type":"step_finish","sessionID":"ses_abc12345","part":{"type":"step-finish","reason":"tool-calls"}}',
            '{"type":"step_start","sessionID":"ses_abc12345","part":{"type":"step-start","messageID":"m2"}}',
            '{"type":"text","sessionID":"ses_abc12345","part":{"type":"text","messageID":"m2","text":"\\u001b[0m当前活跃会话：\\n- `agy` (3 windows)\\n- `cc` (5 windows)"}}',
            '{"type":"step_finish","sessionID":"ses_abc12345","part":{"type":"step-finish","reason":"stop"}}',
        ])
        parsed, sid = node.parse_opencode_json_result(stream)
        self.assertEqual(parsed, "当前活跃会话：\n- `agy` (3 windows)\n- `cc` (5 windows)")
        self.assertEqual(sid, "ses_abc12345")
        self.assertEqual(node.parse_opencode_json(stream), parsed)
        self.assertIsNone(node.parse_opencode_json("plain text output"))

    def test_summarize_progress_line_formats_opencode_events(self) -> None:
        ev = '{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"title":"tmux ls","input":{"command":"tmux ls"}}}}'
        self.assertEqual(node.summarize_progress_line("opencode", ev), "bash: tmux ls")
        self.assertEqual(
            node.summarize_progress_line("opencode", '{"type":"step_start","part":{"type":"step-start"}}'),
            "",
        )
        self.assertEqual(node.summarize_progress_line("claude", "\x1b[31mhi\x1b[0m"), "hi")

    def test_parse_claude_stream_json_and_progress(self) -> None:
        stream = "\n".join([
            '{"type":"system","subtype":"init","session_id":"cl_ses_001"}',
            '{"type":"assistant","session_id":"cl_ses_001","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git status"}}]}}',
            '{"type":"assistant","session_id":"cl_ses_001","message":{"content":[{"type":"text","text":"Working tree is clean."}]}}',
            '{"type":"result","subtype":"success","session_id":"cl_ses_001","result":"All checks passed."}',
        ])
        text, sid = node.parse_claude_stream_json(stream)
        self.assertEqual(text, "All checks passed.")
        self.assertEqual(sid, "cl_ses_001")
        self.assertEqual(
            node.summarize_progress_line("claude", stream.splitlines()[1]),
            "Bash: git status",
        )
        self.assertEqual(node.summarize_progress_line("claude", stream.splitlines()[0]), "")
        self.assertEqual(node.parse_cli_output("claude", "plain fallback"), (None, None))

    def test_parse_codex_json_and_progress(self) -> None:
        stream = "\n".join([
            '{"type":"thread.started","thread_id":"019a0000-1111-2222-3333-444455556666"}',
            '{"type":"turn.started"}',
            '{"type":"item.started","item":{"id":"i1","type":"command_execution","command":"cargo test"}}',
            '{"type":"item.completed","item":{"id":"i2","type":"agent_message","text":"20 tests passed."}}',
        ])
        text, sid = node.parse_codex_json_result(stream)
        self.assertEqual(text, "20 tests passed.")
        self.assertEqual(sid, "019a0000-1111-2222-3333-444455556666")
        self.assertEqual(
            node.summarize_progress_line("codex", stream.splitlines()[2]),
            "bash: cargo test",
        )
        self.assertEqual(node.summarize_progress_line("codex", stream.splitlines()[1]), "")

    def test_parse_agy_stream_json_and_progress(self) -> None:
        stream = "\n".join([
            '{"event":"init","conversation_id":"4c7141d8-84b2-4dd3-9dbc-c17204891d47","init":{"cwd":"/w"}}',
            '{"event":"step_update","step_update":{"conversation_id":"4c7141d8-84b2-4dd3-9dbc-c17204891d47","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"Checking..."}}',
            '{"event":"step_update","step_update":{"conversation_id":"4c7141d8-84b2-4dd3-9dbc-c17204891d47","step_index":2,"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"name":"run_command","parameters":{"CommandLine":"ls -la"}}}}',
            '{"event":"step_update","step_update":{"conversation_id":"4c7141d8-84b2-4dd3-9dbc-c17204891d47","step_index":3,"state":"ACTIVE","step_type":"agent_response","text_delta":"Done: 2 files found."}}',
            '{"event":"result","result":{"conversation_id":"4c7141d8-84b2-4dd3-9dbc-c17204891d47","status":"SUCCESS","response":"Done: 2 files found.\\n"}}',
        ])
        text, sid = node.parse_agy_stream_json(stream)
        self.assertEqual(text, "Done: 2 files found.")
        self.assertEqual(sid, "4c7141d8-84b2-4dd3-9dbc-c17204891d47")
        self.assertEqual(node.parse_cli_output("agy", stream), ("Done: 2 files found.", "4c7141d8-84b2-4dd3-9dbc-c17204891d47"))
        self.assertEqual(
            node.summarize_progress_line("agy", stream.splitlines()[2]),
            "run_command: ls -la",
        )
        self.assertEqual(node.summarize_progress_line("agy", stream.splitlines()[0]), "")

        denied_stream = "\n".join([
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}',
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}}',
        ])
        d_text, d_sid = node.parse_agy_stream_json(denied_stream)
        self.assertIn("Bash", d_text or "")
        self.assertEqual(d_sid, "811f01b5-3dcf-46f7-a376-4af546dae32c")
        self.assertTrue(node.detect_cli_stream_error("agy", denied_stream))
        self.assertFalse(node.detect_cli_stream_error("agy", stream))
        self.assertEqual(
            node.summarize_progress_line("agy", denied_stream.splitlines()[1]),
            "Action denied: Bash",
        )

        partial_denied_stream = "\n".join([
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}',
            '{"event":"step_update","step_update":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"I\'ll start with a read-only survey..."}}',
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}}',
        ])
        pd_text, _ = node.parse_agy_stream_json(partial_denied_stream)
        self.assertIn("I'll start with a read-only survey...", pd_text or "")
        self.assertIn("Action denied in headless mode: Bash", pd_text or "")
        self.assertTrue(node.detect_cli_stream_error("agy", partial_denied_stream))

        err_status_stream = "\n".join([
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}',
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"ERROR","response":""}}',
        ])
        es_text, _ = node.parse_agy_stream_json(err_status_stream)
        self.assertIn("ERROR", es_text or "")
        self.assertTrue(node.detect_cli_stream_error("agy", err_status_stream))

    def test_build_cli_argv_supports_resume_and_sandbox(self) -> None:
        cl = node.build_cli_argv("claude", "/bin/claude", "hi", "/w", "readonly", "/ev", "/in", None, "sid_1")
        self.assertEqual(cl[:6], ["/bin/claude", "-p", "--output-format", "stream-json", "--verbose", "--resume"])
        self.assertIn("sid_1", cl)
        self.assertEqual(cl[cl.index("--allowedTools") + 1], "Read,Glob,Grep")
        self.assertEqual(cl[cl.index("--disallowedTools") + 1], "Bash,Edit,Write,NotebookEdit")

        cl_wr = node.build_cli_argv("claude", "/bin/claude", "hi", "/w", "workspace-write", "/ev", None)
        self.assertEqual(cl_wr[cl_wr.index("--allowedTools") + 1], "Bash,Read,Edit,Write,Glob,Grep")
        self.assertNotIn("--disallowedTools", cl_wr)

        cl_full = node.build_cli_argv("claude", "/bin/claude", "hi", "/w", "unsafe-full", "/ev", None)
        self.assertIn("--dangerously-skip-permissions", cl_full)
        self.assertNotIn("--allowedTools", cl_full)

        cx_new = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "")
        self.assertEqual(cx_new[:4], ["/bin/codex", "exec", "--skip-git-repo-check", "--json"])
        self.assertIn("-s", cx_new)
        self.assertIn("-C", cx_new)
        self.assertIn("sandbox_workspace_write.network_access=true", cx_new)

        cx_ro = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "readonly", "/ev", None, "/tmp/o.txt", "")
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_ro)
        cx_full = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "unsafe-full", "/ev", None, "/tmp/o.txt", "")
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_full)

        cx_res = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "th_1")
        self.assertEqual(cx_res[:3], ["/bin/codex", "exec", "resume"])
        self.assertIn('sandbox_mode="workspace-write"', cx_res)
        self.assertIn("sandbox_workspace_write.network_access=true", cx_res)
        self.assertNotIn("-s", cx_res)
        self.assertNotIn("-C", cx_res)
        self.assertEqual(cx_res[-2:], ["th_1", "hi"])

        ag_ro = node.build_cli_argv("agy", "/bin/agy", "hi", "/w", "readonly", "/ev", "/in", None, "ag_1")
        self.assertIn("--sandbox", ag_ro)
        self.assertIn("--settings", ag_ro)
        self.assertNotIn("--add-dir", ag_ro)
        self.assertNotIn("--mode", ag_ro)
        self.assertNotIn("--dangerously-skip-permissions", ag_ro)
        ag_ro_cfg = json.loads(ag_ro[ag_ro.index("--settings") + 1])
        self.assertTrue(ag_ro_cfg["enableTerminalSandbox"])
        self.assertEqual(ag_ro_cfg["toolPermission"], "proceed-in-sandbox")
        self.assertEqual(ag_ro_cfg["permissions"]["allow"], ["read_file(/w)", "read_file(/in)"])
        self.assertIn("write_file(*)", ag_ro_cfg["permissions"]["deny"])
        self.assertIn("unsandboxed(*)", ag_ro_cfg["permissions"]["deny"])
        self.assertIn("command(git commit)", ag_ro_cfg["permissions"]["deny"])
        self.assertNotIn("command(*)", ag_ro_cfg["permissions"]["deny"])
        self.assertEqual(ag_ro[-2:], ["--conversation", "ag_1"])

        ag_wr = node.build_cli_argv("agy", "/bin/agy", "hi", "/w", "workspace-write", "/ev", None)
        self.assertIn("--mode", ag_wr)
        self.assertEqual(ag_wr[ag_wr.index("--mode") + 1], "accept-edits")
        self.assertIn("--sandbox", ag_wr)
        self.assertIn("--settings", ag_wr)
        self.assertNotIn("--dangerously-skip-permissions", ag_wr)
        ag_wr_cfg = json.loads(ag_wr[ag_wr.index("--settings") + 1])
        self.assertEqual(ag_wr_cfg, {"enableTerminalSandbox": True, "toolPermission": "proceed-in-sandbox"})

        ag_full = node.build_cli_argv("agy", "/bin/agy", "hi", "/w", "unsafe-full", "/ev", None)
        self.assertIn("--dangerously-skip-permissions", ag_full)
        self.assertNotIn("--sandbox", ag_full)
        self.assertNotIn("--settings", ag_full)

        oc = node.build_cli_argv("opencode", "/bin/opencode", "hi", "/w", "workspace-write", "/ev", None, None, "oc_1")
        self.assertEqual(oc, ["/bin/opencode", "run", "--format", "json", "--session", "oc_1", "hi"])
        self.assertNotIn("--auto", oc)
        self.assertNotIn("OPENCODE_PERMISSION", node.build_cli_env("opencode", {}, "workspace-write"))
        oc_ro_env = node.build_cli_env("opencode", {}, "readonly")
        self.assertEqual(json.loads(oc_ro_env["OPENCODE_PERMISSION"]), {"edit": "deny", "bash": "deny"})
        oc_full = node.build_cli_argv("opencode", "/bin/opencode", "hi", "/w", "unsafe-full", "/ev", None)
        self.assertIn("--auto", oc_full)

    def test_tighten_sandbox_only_tightens_never_escalates(self) -> None:
        self.assertEqual(node.tighten_sandbox("workspace-write", ""), "workspace-write")
        self.assertEqual(node.tighten_sandbox("workspace-write", "readonly"), "readonly")
        self.assertEqual(node.tighten_sandbox("workspace-write", "read-only"), "readonly")
        self.assertEqual(node.tighten_sandbox("workspace-write", "unsafe-full"), "workspace-write")
        self.assertEqual(node.tighten_sandbox("readonly", "workspace-write"), "readonly")
        self.assertEqual(node.tighten_sandbox("readonly", "unsafe-full"), "readonly")
        self.assertEqual(node.tighten_sandbox("unsafe-full", "readonly"), "readonly")
        self.assertEqual(node.tighten_sandbox("unsafe-full", "workspace-write"), "workspace-write")

    def test_sanitize_job_model_and_effort_reject_unsafe_inputs(self) -> None:
        self.assertEqual(node.sanitize_job_model("  claude-opus-4-6  "), "claude-opus-4-6")
        self.assertEqual(node.sanitize_job_model("openai/gpt-6.2-sol"), "openai/gpt-6.2-sol")
        self.assertEqual(node.sanitize_job_model("-bad-flag"), "")
        self.assertEqual(node.sanitize_job_model("gpt 4o"), "")
        self.assertEqual(node.sanitize_job_model("m" * 129), "")
        self.assertEqual(node.sanitize_job_effort(" HIGH "), "high")
        self.assertEqual(node.sanitize_job_effort("invalid"), "")

    def test_build_cli_argv_passes_model_and_effort_across_engines(self) -> None:
        cl = node.build_cli_argv(
            "claude", "/bin/claude", "hi", "/w", "readonly", "/ev", None, None, "s1",
            model="opus-5.5", effort="minimal",
        )
        self.assertIn("--model", cl)
        self.assertEqual(cl[cl.index("--model") + 1], "opus-5.5")
        self.assertIn("--effort", cl)
        self.assertEqual(cl[cl.index("--effort") + 1], "low")

        cx_new = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "",
            model="gpt-6.2-sol", effort="max",
        )
        self.assertEqual(cx_new[cx_new.index("-m") + 1], "gpt-6.2-sol")
        self.assertIn('model_reasoning_effort="xhigh"', cx_new)

        cx_res = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "th_1",
            model="gpt-6.2-sol", effort="medium",
        )
        self.assertEqual(cx_res[:3], ["/bin/codex", "exec", "resume"])
        self.assertEqual(cx_res[cx_res.index("-m") + 1], "gpt-6.2-sol")
        self.assertIn('model_reasoning_effort="medium"', cx_res)

        ag = node.build_cli_argv(
            "agy", "/bin/agy", "hi", "/w", "readonly", "/ev", None, None, "",
            model="claude-opus-5-5-high", effort="minimal",
        )
        self.assertEqual(ag[ag.index("--model") + 1], "claude-opus-5-5-high")
        self.assertEqual(ag[ag.index("--effort") + 1], "low")

        oc = node.build_cli_argv(
            "opencode", "/bin/opencode", "hi", "/w", "workspace-write", "/ev", None, None, "oc_1",
            model="openai/o3", effort="xhigh",
        )
        self.assertEqual(oc[oc.index("-m") + 1], "openai/o3")
        self.assertEqual(oc[oc.index("--variant") + 1], "high")

    def test_resolve_node_agents_multi_and_pinned(self) -> None:
        with mock.patch.object(node.shutil, "which", side_effect=lambda c: f"/usr/bin/{c}" if c in ("claude", "opencode") else None), mock.patch.dict(os.environ, {}, clear=True):
            # Explicit --agent without --agents stays pinned to that engine for safety.
            self.assertEqual(node.resolve_node_agents("opencode", ""), ("opencode", ["opencode"]))
            # --agent auto discovers all installed engines.
            self.assertEqual(node.resolve_node_agents("auto", ""), ("claude", ["claude", "opencode"]))
            # --agents auto with preferred default keeps opencode default while exposing all discovered.
            self.assertEqual(node.resolve_node_agents("opencode", "auto"), ("opencode", ["claude", "opencode"]))
            # Explicit comma allowlist respects default and dedupes.
            self.assertEqual(
                node.resolve_node_agents("codex", "claude, opencode, claude"),
                ("codex", ["codex", "claude", "opencode"]),
            )
            self.assertEqual(node.resolve_node_agents("auto", "", "echo ok"), ("custom:echo ok", ["custom:echo ok"]))

    def test_select_job_agent_enforces_allowlist(self) -> None:
        self.assertEqual(node.select_job_agent(None, "opencode", ["claude", "opencode"]), ("opencode", ""))
        self.assertEqual(node.select_job_agent("", "opencode", ["claude", "opencode"]), ("opencode", ""))
        self.assertEqual(node.select_job_agent("CLAUDE", "opencode", ["claude", "opencode"]), ("claude", ""))
        chosen, err = node.select_job_agent("agy", "opencode", ["claude", "opencode"])
        self.assertIsNone(chosen)
        self.assertIn("not enabled", err)

    def test_build_env_header_includes_agents_before_repos(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "repo-a", ".git"))
            hdr = node.build_env_header("pc", "opencode", tmp, ["claude", "opencode"])
            self.assertIn("agent:opencode", hdr)
            self.assertIn("agents:claude,opencode", hdr)
            self.assertIn("repos:repo-a", hdr)
            self.assertLess(hdr.index("agents:"), hdr.index("repos:"))

    def test_execute_job_marks_agy_headless_denial_with_rc0_as_failed(self) -> None:
        denied_stream = (
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}\n'
            '{"event":"step_update","step_update":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","step_index":1,"state":"DONE","step_type":"agent_response","text_delta":"I\'ll start with a read-only survey..."}}\n'
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}}\n'
        ).encode("utf-8")
        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/agy"),
                mock.patch.object(node, "run_monitored", return_value=(0, denied_stream, b"", False)),
            ):
                status, content, _ = node.execute_job(
                    "agy",
                    {"task_id": "tsk_deny", "title": "Survey", "instruction": "Check repo", "sandbox": "readonly"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                )
        self.assertEqual(status, "failed")
        self.assertIn("I'll start with a read-only survey...", content)
        self.assertIn("Action denied in headless mode: Bash", content)

    def test_execute_job_agy_readonly_uses_isolated_temp_cwd_and_cleans_up(self) -> None:
        captured_cwds: list[str] = []
        ok_stream = (
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}\n'
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"SUCCESS","response":"AGY-OK"}}\n'
        ).encode("utf-8")

        def fake_run(_argv, **kwargs):
            captured_cwds.append(kwargs["cwd"])
            self.assertTrue(os.path.isdir(kwargs["cwd"]))
            return (0, ok_stream, b"", False)

        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/agy"),
                mock.patch.object(node, "run_monitored", side_effect=fake_run),
            ):
                status, content, _ = node.execute_job(
                    "agy",
                    {"task_id": "tsk_ro_cwd", "title": "Survey", "instruction": "Check repo", "sandbox": "readonly"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                )
        self.assertEqual(status, "completed")
        self.assertEqual(content, "AGY-OK")
        self.assertEqual(len(captured_cwds), 1)
        self.assertNotEqual(captured_cwds[0], workdir)
        self.assertIn("agenct-ro-", os.path.basename(captured_cwds[0]))
        self.assertFalse(os.path.exists(captured_cwds[0]))

    def test_build_prompt_readonly_hint_scoped_to_readonly_shell_disabled_engines(self) -> None:
        job = {"title": "Plan", "instruction": "Read files"}
        agy_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="agy")
        claude_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="claude")
        opencode_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="opencode")
        codex_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="codex")
        agy_wr = node.build_prompt(job, [], "/ev", "/w", sandbox="workspace-write", agent="agy")
        self.assertNotIn("file writes and shell commands are disabled", agy_ro)
        self.assertIn("mounted read-only", agy_ro)
        self.assertIn("cd /w", agy_ro)
        self.assertIn("file writes and shell commands are disabled", claude_ro)
        self.assertIn("file writes and shell commands are disabled", opencode_ro)
        self.assertNotIn("Read-only sandbox is active", codex_ro)
        self.assertNotIn("Read-only sandbox is active", agy_wr)

    def test_all_cli_adapters_satisfy_contract(self) -> None:
        job = {"title": "Contract", "instruction": "Read files"}
        for eng in node.CLI_AGENTS:
            with self.subTest(engine=eng):
                self.assertIn(eng, node.CLI_ADAPTERS)
                adapter = node.CLI_ADAPTERS[eng]
                ro = (
                    tuple(node.build_cli_argv(eng, f"/bin/{eng}", "p", "/w", "readonly", "/ev", None)),
                    tuple(sorted(node.build_cli_env(eng, {}, "readonly").items())),
                )
                wr = (
                    tuple(node.build_cli_argv(eng, f"/bin/{eng}", "p", "/w", "workspace-write", "/ev", None)),
                    tuple(sorted(node.build_cli_env(eng, {}, "workspace-write").items())),
                )
                full = (
                    tuple(node.build_cli_argv(eng, f"/bin/{eng}", "p", "/w", "unsafe-full", "/ev", None)),
                    tuple(sorted(node.build_cli_env(eng, {}, "unsafe-full").items())),
                )
                self.assertNotEqual(ro, wr)
                self.assertNotEqual(wr, full)
                self.assertEqual(node.parse_cli_output(eng, "plain text"), (None, None))
                self.assertFalse(node.detect_cli_stream_error(eng, "plain text"))
                prompt = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent=eng)
                if adapter.readonly_disables_shell:
                    self.assertIn("file writes and shell commands are disabled", prompt)
                else:
                    self.assertNotIn("file writes and shell commands are disabled", prompt)

    def test_stream_errors_and_denials_detected_across_all_cli_engines(self) -> None:
        cl_err = "\n".join([
            '{"type":"assistant","session_id":"cl_1","message":{"content":[{"type":"text","text":"Starting..."}]}}',
            '{"type":"result","subtype":"success","is_error":true,"session_id":"cl_1","result":"Partial","permission_denials":[{"tool_name":"Bash"}]}',
        ])
        cl_text, cl_sid = node.parse_claude_stream_json(cl_err)
        self.assertEqual(cl_sid, "cl_1")
        self.assertIn("Partial", cl_text or "")
        self.assertIn("Action denied in headless mode: Bash", cl_text or "")
        self.assertTrue(node.detect_cli_stream_error("claude", cl_err))

        cx_err = "\n".join([
            '{"type":"thread.started","thread_id":"019a0000-1111-2222-3333-444455556666"}',
            '{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Checking repo..."}}',
            '{"type":"turn.failed","error":{"message":"Sandbox blocked operation"}}',
        ])
        cx_text, _ = node.parse_codex_json_result(cx_err)
        self.assertIn("Checking repo...", cx_text or "")
        self.assertIn("Sandbox blocked operation", cx_text or "")
        self.assertTrue(node.detect_cli_stream_error("codex", cx_err))

        oc_err = "\n".join([
            '{"type":"text","sessionID":"ses_1234","part":{"type":"text","messageID":"m1","text":"Looking..."}}',
            '{"type":"error","sessionID":"ses_1234","error":{"data":{"message":"Model rate limited"}}}',
        ])
        oc_text, _ = node.parse_opencode_json_result(oc_err)
        self.assertIn("Looking...", oc_text or "")
        self.assertIn("Model rate limited", oc_text or "")
        self.assertTrue(node.detect_cli_stream_error("opencode", oc_err))

        oc_perm_line = "! permission requested: external_directory (/tmp/foo/*); auto-rejecting"
        self.assertTrue(node.detect_cli_stream_error("opencode", '{"type":"step_start"}\n', oc_perm_line))
        self.assertFalse(node.detect_cli_stream_error("opencode", '{"type":"text","part":{"text":"Do not trigger on ! permission requested prose"}}\n', ""))
        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/opencode"),
                mock.patch.object(
                    node,
                    "run_monitored",
                    return_value=(0, b'{"type":"text","sessionID":"ses_1","part":{"messageID":"m1","text":"ok"}}\n', oc_perm_line.encode("utf-8"), False),
                ),
            ):
                status, _, _ = node.execute_job(
                    "opencode",
                    {"task_id": "tsk_oc", "title": "Check", "instruction": "Hi"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                )
        self.assertEqual(status, "failed")

    def test_parse_args_self_check_does_not_require_hub(self) -> None:
        args = node.parse_args(["--self-check", "--agents", "codex,agy"])
        self.assertTrue(args.self_check)
        self.assertEqual(args.agents, "codex,agy")
        self.assertEqual(args.hub, "")
        with self.assertRaises(SystemExit):
            node.parse_args(["--agents", "codex"])

    def test_run_self_check_mocked_pass_and_fail(self) -> None:
        with tempfile.TemporaryDirectory() as workdir:
            def fake_exec(agent, job, wd, _sb, _to, _ev, _cb):
                inst = job["instruction"]
                if job.get("sandbox") == "readonly":
                    path = inst.split("Read ", 1)[1].split(" and reply", 1)[0]
                    with open(os.path.join(wd, path), "r", encoding="utf-8") as fh:
                        return ("completed", fh.read().strip(), [])
                token = inst.split("print ", 1)[1].split(" (e.g.", 1)[0]
                return ("completed", token, [])

            args = node.parse_args(["--self-check", "--workdir", workdir, "--agents", "codex,agy"])
            with (
                mock.patch.object(node, "execute_job", side_effect=fake_exec),
                mock.patch.object(node, "resolve_node_agents", return_value=("codex", ["codex", "agy"])),
            ):
                self.assertEqual(node.run_self_check(args), 0)

            with (
                mock.patch.object(node, "execute_job", return_value=("failed", "Action denied", [])),
                mock.patch.object(node, "resolve_node_agents", return_value=("agy", ["agy"])),
            ):
                self.assertEqual(node.run_self_check(args), 1)


if __name__ == "__main__":
    unittest.main()
