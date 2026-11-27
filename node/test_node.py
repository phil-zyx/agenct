"""Unit tests for the unified Agenct execution node (node/agenct-node.py)."""
from __future__ import annotations

import base64
import importlib.util
import io
import json
import os
import shlex
import subprocess
import sys
import tempfile
import threading
import unittest
from contextlib import redirect_stderr, redirect_stdout
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
        self.assertFalse(args.codex_tmux)
        self.assertEqual(args.kind, "pc")
        self.assertEqual(args.wait, 25)

    def test_node_timeout_default_and_overrides(self) -> None:
        argv = ["--self-check"]
        with mock.patch.dict(os.environ, clear=True):
            self.assertEqual(node.parse_args(argv).timeout, 3600)
        with mock.patch.dict(os.environ, {"AGENCT_NODE_TIMEOUT": "7200"}):
            self.assertEqual(node.parse_args(argv).timeout, 7200)
            self.assertEqual(node.parse_args([*argv, "--timeout", "45"]).timeout, 45)

    def test_node_timeout_invalid_configuration(self) -> None:
        argv = ["--self-check"]
        for value in ("", "invalid", "1.5", "0", "-1"):
            with (
                self.subTest(value=value),
                mock.patch.dict(os.environ, {"AGENCT_NODE_TIMEOUT": value}),
                redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(node.parse_args(argv).timeout, 3600)
                self.assertIn("invalid AGENCT_NODE_TIMEOUT", stderr.getvalue())
                self.assertEqual(node.parse_args([*argv, "--timeout", "30"]).timeout, 30)
                with self.assertRaises(SystemExit) as exc:
                    node.parse_args([*argv, "--timeout", value])
                self.assertEqual(exc.exception.code, 2)
                self.assertIn("expected a positive integer", stderr.getvalue())

    def test_job_timeout_kills_process_group_and_returns_partial_output(self) -> None:
        script = (
            "import sys, time; "
            "print('partial stdout', end='', flush=True); "
            "print('partial stderr', end='', file=sys.stderr, flush=True); "
            "time.sleep(5)"
        )
        command = shlex.join([sys.executable, "-c", script]) + " & wait"
        with tempfile.TemporaryDirectory() as workdir, mock.patch.object(node.os, "killpg", wraps=os.killpg) as killpg:
            status, content, _ = node.execute_job(
                f"custom:{command}", {"tmux_session": "job-123"}, workdir, "workspace-write",
                1, threading.Event(), lambda _s, _t: None,
            )
        killpg.assert_called_once()
        pgid, sig = killpg.call_args.args
        self.assertNotEqual(pgid, os.getpgrp())
        self.assertEqual(sig, node.signal.SIGKILL)
        self.assertEqual(status, "timeout")
        self.assertIn("[timeout] Execution timed out after 1s. Adjust AGENCT_NODE_TIMEOUT", content)
        self.assertIn("stdout:\npartial stdout", content)
        self.assertIn("stderr:\npartial stderr", content)
        self.assertIn("Tmux session: job-123", content)

    def test_node_idle_timeout_default_and_overrides(self) -> None:
        argv = ["--self-check"]
        with mock.patch.dict(os.environ, clear=True):
            self.assertEqual(node.parse_args(argv).idle_timeout, 300)
        with mock.patch.dict(os.environ, {"AGENCT_NODE_IDLE_TIMEOUT": "180"}):
            self.assertEqual(node.parse_args(argv).idle_timeout, 180)
            self.assertEqual(node.parse_args([*argv, "--idle-timeout", "0"]).idle_timeout, 0)
            self.assertEqual(node.parse_args([*argv, "--idle-timeout", "45"]).idle_timeout, 45)
        for value in ("", "invalid", "1.5", "-1"):
            with (
                self.subTest(value=value),
                mock.patch.dict(os.environ, {"AGENCT_NODE_IDLE_TIMEOUT": value}),
                redirect_stderr(io.StringIO()) as stderr,
            ):
                self.assertEqual(node.parse_args(argv).idle_timeout, 300)
                self.assertIn("invalid AGENCT_NODE_IDLE_TIMEOUT", stderr.getvalue())
                with self.assertRaises(SystemExit) as exc:
                    node.parse_args([*argv, "--idle-timeout", value])
                self.assertEqual(exc.exception.code, 2)
                self.assertIn("expected a non-negative integer", stderr.getvalue())

    def test_job_idle_timeout_kills_silent_process_group(self) -> None:
        script = (
            "import time; "
            "print('initial line', flush=True); "
            "time.sleep(5)"
        )
        command = shlex.join([sys.executable, "-c", script]) + " & wait"
        with (
            tempfile.TemporaryDirectory() as workdir,
            mock.patch.object(node.os, "killpg", wraps=os.killpg) as killpg,
        ):
            t0 = node.time.monotonic()
            status, content, _ = node.execute_job(
                f"custom:{command}", {"tmux_session": "job-idle"}, workdir, "workspace-write",
                10, threading.Event(), lambda _s, _t: None, idle_timeout=1,
            )
            elapsed = node.time.monotonic() - t0
        self.assertLess(elapsed, 4.0)
        killpg.assert_called_once()
        pgid, sig = killpg.call_args.args
        self.assertNotEqual(pgid, os.getpgrp())
        self.assertEqual(sig, node.signal.SIGKILL)
        self.assertEqual(status, "timeout")
        self.assertIn("[idle-timeout] No output for 1s", content)
        self.assertIn("stdout:\ninitial line", content)

    def test_job_active_output_resets_idle_timeout(self) -> None:
        script = (
            "import time\n"
            "for i in range(4):\n"
            "    print(f'tick-{i}', flush=True)\n"
            "    if i < 3:\n"
            "        time.sleep(0.4)\n"
        )
        command = shlex.join([sys.executable, "-c", script])
        with tempfile.TemporaryDirectory() as workdir:
            status, content, _ = node.execute_job(
                f"custom:{command}", {}, workdir, "workspace-write",
                10, threading.Event(), lambda _s, _t: None, idle_timeout=1,
            )
        self.assertEqual(status, "completed")
        self.assertIn("tick-3", content)

    def test_job_idle_timeout_zero_disables_watchdog(self) -> None:
        script = "import time; time.sleep(1.2); print('ok', flush=True)"
        command = shlex.join([sys.executable, "-c", script])
        with tempfile.TemporaryDirectory() as workdir:
            status, content, _ = node.execute_job(
                f"custom:{command}", {}, workdir, "workspace-write",
                10, threading.Event(), lambda _s, _t: None, idle_timeout=0,
            )
        self.assertEqual(status, "completed")
        self.assertIn("ok", content)

    def test_node_submits_timeout_with_partial_output_and_agent_session(self) -> None:
        job = {"task_id": "tsk_timeout", "task_revision": 1, "lease_token": "lease", "agent": "codex"}
        partial = b'{"type":"thread.started","thread_id":"thread-1234"}\n'

        def fake_http(url, _token, *, method, **kwargs):
            if url.endswith("/work"):
                return {"job": job}
            if url.endswith("/submit"):
                self.assertEqual(method, "POST")
                payload = kwargs["body"]
                self.assertEqual(payload["status"], "failed")
                self.assertEqual(payload["agent_session_id"], "thread-1234")
                self.assertIn("[timeout] Execution timed out after 2s", payload["content"])
                self.assertIn(partial.decode().strip(), payload["content"])
                self.assertIn("partial stderr", payload["content"])
                return {}
            self.fail(f"Unexpected request: {url}")

        with tempfile.TemporaryDirectory() as workdir:
            args = node.parse_args([
                "--hub", "https://agenct.example.com", "--token", "test-token", "--once",
                "--agent", "codex", "--agents", "codex", "--workdir", workdir,
                "--state-dir", workdir, "--timeout", "2",
            ])
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/codex"),
                mock.patch.object(node, "run_monitored",
                                  side_effect=subprocess.TimeoutExpired("codex", 2, partial, b"partial stderr")),
                mock.patch.object(node, "http_json", side_effect=fake_http) as http,
            ):
                self.assertEqual(node.run_node_loop(args), 0)
                self.assertEqual(http.call_args.args[0], "https://agenct.example.com/node/v1/tasks/tsk_timeout/submit")
            self.assertEqual(node.ExecutionJournal(workdir).pending_submissions(), [])

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

    def test_custom_cmd_succeeds_and_unsupported_engine_fails(self) -> None:
        with tempfile.TemporaryDirectory() as workdir:
            import threading

            status, content, atts = node.execute_job(
                'custom:python3 -c "print(\'Custom OK\')"',
                {"task_id": "tsk_1", "title": "Hello", "instruction": "Inspect repo"},
                workdir,
                "workspace-write",
                10,
                threading.Event(),
                lambda _s, _t: None,
            )
            self.assertEqual(status, "completed")
            self.assertIn("Custom OK", content)
            self.assertEqual(atts, [])

            bad_status, bad_content, bad_atts = node.execute_job(
                "template",
                {"task_id": "tsk_2", "title": "Bad", "instruction": "Nope"},
                workdir,
                "workspace-write",
                10,
                threading.Event(),
                lambda _s, _t: None,
            )
            self.assertEqual(bad_status, "failed")
            self.assertIn("Unsupported execution engine", bad_content)
            self.assertEqual(bad_atts, [])

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
        parsed, sid = node.parse_cli_output("opencode", stream)
        self.assertEqual(parsed, "当前活跃会话：\n- `agy` (3 windows)\n- `cc` (5 windows)")
        self.assertEqual(sid, "ses_abc12345")
        self.assertEqual(node.parse_cli_output("opencode", "plain text output"), (None, None))

    def test_summarize_progress_line_formats_opencode_events(self) -> None:
        ev = '{"type":"tool_use","part":{"type":"tool","tool":"bash","state":{"title":"tmux ls","input":{"command":"tmux ls"}}}}'
        self.assertEqual(node.summarize_progress_line("opencode", ev), "bash: tmux ls")
        self.assertEqual(
            node.summarize_progress_line("opencode", '{"type":"step_start","part":{"type":"step-start"}}'),
            "",
        )
        self.assertEqual(node.summarize_progress_line("claude", "\x1b[31mhi\x1b[0m"), "hi")

    def test_format_progress_entry_and_run_monitored_dedup_no_midline_slice(self) -> None:
        wd = "/tmp/ws-root"
        self.assertEqual(node.format_progress_entry("read_file: /tmp/ws-root/hub/web/style.css", wd), "查看 hub/web/style.css")
        self.assertEqual(node.format_progress_entry("replace_file_content: /tmp/ws-root/a.js", wd), "修改 a.js")
        self.assertEqual(node.format_progress_entry("run_command: cd /tmp/ws-root && grep -rn foo bar | head -n 5", wd), "执行 grep -rn foo bar | head -n 5")
        self.assertEqual(node.format_progress_entry("update_steps: ...", wd), "")
        self.assertEqual(node.format_progress_entry("exit_plan_mode: ...", wd), "规划完成，开始执行")
        long_entry = node.format_progress_entry("run_command: " + ("echo x && " * 20), wd, max_chars=40)
        self.assertTrue(long_entry.startswith("执行 "))
        self.assertTrue(long_entry.endswith("…"))
        self.assertLessEqual(len(long_entry), 40)

        agy_active = '{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"tool","tool_name":"read_file","tool_info":{"parameters":{"Path":"/tmp/ws-root/a.css"}}}}'
        agy_done = '{"event":"step_update","step_update":{"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":{"parameters":{"Path":"/tmp/ws-root/a.css"}}}}'
        agy_summary = '{"event":"step_update","step_update":{"state":"ACTIVE","step_type":"tool","tool_name":"run_command","tool_info":{"parameters":{"toolSummary":"Check z-index styles","CommandLine":"cd /tmp/ws-root && grep x"}}}}'
        self.assertEqual(node.summarize_progress_line("agy", agy_active), "read_file: /tmp/ws-root/a.css")
        self.assertEqual(node.summarize_progress_line("agy", agy_done), "")
        self.assertEqual(node.summarize_progress_line("agy", agy_summary), "Check z-index styles")

        events_seen: list[str] = []

        class StubLog(node.BackendLogStream):
            def __init__(self) -> None:
                self._sent = False

            def read_chunks(self):
                if self._sent:
                    return b"", b""
                self._sent = True
                return (
                    b"read_file: /tmp/ws-root/a.css\n"
                    b"read_file: /tmp/ws-root/a.css\n"
                    b"run_command: cd /tmp/ws-root && grep a b | grep c\n",
                    b"",
                )

            def is_done(self) -> bool:
                return self._sent

            def returncode(self) -> int | None:
                return 0 if self._sent else None

        class StubBackend(node.ExecBackend):
            name = "stub"

            def spawn(self, argv, *, shell, cwd, env, task_id=""):
                return node.ExecHandle(backend=self.name, session_id="s1", argv=argv)

            def log(self, handle):
                return StubLog()

            def attach(self, handle):
                return None

            def cleanup(self, handle, *, terminate=False, force=False):
                pass

            def list(self):
                return []

        node.run_monitored(
            ["noop"], shell=False, cwd=wd, env={}, timeout=5,
            cancel_event=threading.Event(),
            progress_cb=lambda _st, tail: events_seen.append(tail),
            stage_label="Run", backend=StubBackend(), task_id="t_fmt",
        )
        self.assertEqual(
            events_seen[-1],
            "查看 a.css\n执行 grep a b | grep c",
        )

    def test_parse_claude_stream_json_and_progress(self) -> None:
        stream = "\n".join([
            '{"type":"system","subtype":"init","session_id":"cl_ses_001"}',
            '{"type":"assistant","session_id":"cl_ses_001","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"git status"}}]}}',
            '{"type":"assistant","session_id":"cl_ses_001","message":{"content":[{"type":"text","text":"Working tree is clean."}]}}',
            '{"type":"result","subtype":"success","session_id":"cl_ses_001","result":"All checks passed."}',
        ])
        text, sid = node.parse_cli_output("claude", stream)
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
        text, sid = node.parse_cli_output("codex", stream)
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
        text, sid = node.parse_cli_output("agy", stream)
        self.assertEqual(text, "Done: 2 files found.")
        self.assertEqual(sid, "4c7141d8-84b2-4dd3-9dbc-c17204891d47")
        self.assertEqual(
            node.summarize_progress_line("agy", stream.splitlines()[2]),
            "run_command: ls -la",
        )
        self.assertEqual(node.summarize_progress_line("agy", stream.splitlines()[0]), "")

        denied_stream = "\n".join([
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}',
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"SUCCESS","response":"","denied_actions":[{"action":"command","display_name":"Bash"}]}}',
        ])
        d_text, d_sid = node.parse_cli_output("agy", denied_stream)
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
        pd_text, _ = node.parse_cli_output("agy", partial_denied_stream)
        self.assertIn("I'll start with a read-only survey...", pd_text or "")
        self.assertIn("Action denied in headless mode: Bash", pd_text or "")
        self.assertTrue(node.detect_cli_stream_error("agy", partial_denied_stream))

        err_status_stream = "\n".join([
            '{"event":"init","conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c"}',
            '{"event":"result","result":{"conversation_id":"811f01b5-3dcf-46f7-a376-4af546dae32c","status":"ERROR","response":""}}',
        ])
        es_text, _ = node.parse_cli_output("agy", err_status_stream)
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
        self.assertIn("sandbox_workspace_write.network_access=false", cx_new)
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_new)

        cx_new_tmux = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "", codex_tmux=True,
        )
        self.assertIn("sandbox_workspace_write.network_access=true", cx_new_tmux)
        self.assertNotIn("sandbox_workspace_write.network_access=false", cx_new_tmux)

        cx_ro = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "readonly", "/ev", None, "/tmp/o.txt", "")
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_ro)
        cx_ro_tmux = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "readonly", "/ev", None, "/tmp/o.txt", "", codex_tmux=True,
        )
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_ro_tmux)
        cx_full = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "unsafe-full", "/ev", None, "/tmp/o.txt", "")
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_full)
        cx_full_tmux = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "unsafe-full", "/ev", None, "/tmp/o.txt", "", codex_tmux=True,
        )
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_full_tmux)

        cx_res = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "th_1")
        self.assertEqual(cx_res[:3], ["/bin/codex", "exec", "resume"])
        self.assertIn('sandbox_mode="workspace-write"', cx_res)
        self.assertIn("sandbox_workspace_write.network_access=false", cx_res)
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_res)
        self.assertNotIn("-s", cx_res)
        self.assertNotIn("-C", cx_res)
        self.assertEqual(cx_res[-2:], ["th_1", "hi"])

        cx_res_tmux = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, "/tmp/o.txt", "th_1", codex_tmux=True,
        )
        self.assertIn("sandbox_workspace_write.network_access=true", cx_res_tmux)
        self.assertNotIn("sandbox_workspace_write.network_access=false", cx_res_tmux)
        cx_ro_res_tmux = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "readonly", "/ev", None, "/tmp/o.txt", "th_1", codex_tmux=True,
        )
        self.assertNotIn("sandbox_workspace_write.network_access=true", cx_ro_res_tmux)

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
        oc_wr_env = node.build_cli_env("opencode", {}, "workspace-write")
        self.assertEqual(json.loads(oc_wr_env["OPENCODE_PERMISSION"]), {"external_directory": "deny"})
        oc_ro_env = node.build_cli_env("opencode", {}, "readonly")
        self.assertEqual(
            json.loads(oc_ro_env["OPENCODE_PERMISSION"]),
            {"edit": "deny", "bash": "deny", "external_directory": "deny"},
        )
        oc_full = node.build_cli_argv("opencode", "/bin/opencode", "hi", "/w", "unsafe-full", "/ev", None)
        self.assertIn("--auto", oc_full)
        self.assertNotIn(
            "OPENCODE_PERMISSION",
            node.build_cli_env("opencode", {"OPENCODE_PERMISSION": '{"edit":"deny"}'}, "unsafe-full"),
        )

        # When wrapped by an outer OS sandbox (bwrap / seatbelt), all engines run headless without internal friction.
        cl_iso = node.build_cli_argv("claude", "/bin/claude", "hi", "/w", "readonly", "/ev", None, os_isolated=True)
        self.assertIn("--dangerously-skip-permissions", cl_iso)
        self.assertNotIn("--disallowedTools", cl_iso)
        self.assertNotIn(os.path.expanduser("~"), cl_iso)
        cx_iso = node.build_cli_argv("codex", "/bin/codex", "hi", "/w", "readonly", "/ev", None, os_isolated=True)
        self.assertIn("--dangerously-bypass-approvals-and-sandbox", cx_iso)
        self.assertNotIn("-s", cx_iso)
        cx_iso_res = node.build_cli_argv(
            "codex", "/bin/codex", "hi", "/w", "workspace-write", "/ev", None, None, "th_1", os_isolated=True,
        )
        self.assertEqual(cx_iso_res[:3], ["/bin/codex", "exec", "resume"])
        self.assertIn("--dangerously-bypass-approvals-and-sandbox", cx_iso_res)
        ag_iso = node.build_cli_argv("agy", "/bin/agy", "hi", "/w", "readonly", "/ev", "/in", os_isolated=True)
        self.assertIn("--dangerously-skip-permissions", ag_iso)
        self.assertNotIn("--sandbox", ag_iso)
        self.assertNotIn("--settings", ag_iso)
        oc_iso = node.build_cli_argv("opencode", "/bin/opencode", "hi", "/w", "readonly", "/ev", None, os_isolated=True)
        self.assertIn("--auto", oc_iso)
        self.assertNotIn(
            "OPENCODE_PERMISSION",
            node.build_cli_env("opencode", {"OPENCODE_PERMISSION": '{"edit":"deny"}'}, "readonly", os_isolated=True),
        )

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
                    sandbox_backend="cli-native",
                )
        self.assertEqual(status, "completed")
        self.assertEqual(content, "AGY-OK")
        self.assertEqual(len(captured_cwds), 1)
        self.assertNotEqual(captured_cwds[0], workdir)
        self.assertIn("agenct-ro-", os.path.basename(captured_cwds[0]))
        self.assertFalse(os.path.exists(captured_cwds[0]))

        class StubOsSandbox(node.SandboxBackend):
            name = "bwrap"
            os_isolated = True

            def wrap(self, argv, *, shell, cwd, env, policy):
                return ["/usr/bin/bwrap", "--mode", policy.mode, *argv], False, cwd, dict(env)

        iso_cwds: list[str] = []
        iso_argvs: list[list[str]] = []

        def fake_iso_run(argv, **kwargs):
            iso_argvs.append(list(argv))
            iso_cwds.append(kwargs["cwd"])
            return (0, ok_stream, b"", False)

        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/agy"),
                mock.patch.object(node, "run_monitored", side_effect=fake_iso_run),
            ):
                st2, txt2, _ = node.execute_job(
                    "agy",
                    {"task_id": "tsk_ro_os", "title": "Survey", "instruction": "Check repo", "sandbox": "readonly"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    sandbox_backend=StubOsSandbox(),
                )
        self.assertEqual(st2, "completed")
        self.assertEqual(txt2, "AGY-OK")
        self.assertEqual(iso_cwds, [workdir])
        self.assertEqual(iso_argvs[0][:3], ["/usr/bin/bwrap", "--mode", "readonly"])

    def test_build_prompt_readonly_hint_scoped_to_readonly_shell_disabled_engines(self) -> None:
        job = {"title": "Plan", "instruction": "Read files"}
        agy_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="agy", os_isolated=False)
        claude_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="claude", os_isolated=False)
        opencode_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="opencode", os_isolated=False)
        codex_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent="codex", os_isolated=False)
        agy_wr = node.build_prompt(job, [], "/ev", "/w", sandbox="workspace-write", agent="agy", os_isolated=False)
        self.assertNotIn("file writes and shell commands are disabled", agy_ro)
        self.assertIn("mounted read-only", agy_ro)
        self.assertIn("cd /w", agy_ro)
        self.assertIn("file writes and shell commands are disabled", claude_ro)
        self.assertIn("file writes and shell commands are disabled", opencode_ro)
        self.assertNotIn("Read-only sandbox is active", codex_ro)
        self.assertNotIn("Read-only sandbox is active", agy_wr)

        for eng in node.CLI_AGENTS:
            os_ro = node.build_prompt(job, [], "/ev", "/w", sandbox="readonly", agent=eng, os_isolated=True)
            self.assertIn("Read-only OS sandbox is active", os_ro)
            self.assertIn("blocked by the OS", os_ro)
            self.assertNotIn("shell commands are disabled", os_ro)

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
        cl_text, cl_sid = node.parse_cli_output("claude", cl_err)
        self.assertEqual(cl_sid, "cl_1")
        self.assertIn("Partial", cl_text or "")
        self.assertIn("Action denied in headless mode: Bash", cl_text or "")
        self.assertTrue(node.detect_cli_stream_error("claude", cl_err))

        cx_err = "\n".join([
            '{"type":"thread.started","thread_id":"019a0000-1111-2222-3333-444455556666"}',
            '{"type":"item.completed","item":{"id":"i1","type":"agent_message","text":"Checking repo..."}}',
            '{"type":"turn.failed","error":{"message":"Sandbox blocked operation"}}',
        ])
        cx_text, _ = node.parse_cli_output("codex", cx_err)
        self.assertIn("Checking repo...", cx_text or "")
        self.assertIn("Sandbox blocked operation", cx_text or "")
        self.assertTrue(node.detect_cli_stream_error("codex", cx_err))

        oc_err = "\n".join([
            '{"type":"text","sessionID":"ses_1234","part":{"type":"text","messageID":"m1","text":"Looking..."}}',
            '{"type":"error","sessionID":"ses_1234","error":{"data":{"message":"Model rate limited"}}}',
        ])
        oc_text, _ = node.parse_cli_output("opencode", oc_err)
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
            def fake_exec(agent, job, wd, _sb, _to, _ev, _cb, **_kw):
                inst = job["instruction"]
                if job.get("sandbox") == "readonly":
                    path = inst.split("Read ", 1)[1].split(" and reply", 1)[0]
                    with open(os.path.join(wd, path), "r", encoding="utf-8") as fh:
                        return ("completed", fh.read().strip(), [])
                token = inst.split("print ", 1)[1].split(" (e.g.", 1)[0]
                return ("completed", token, [])

            args = node.parse_args(["--self-check", "--workdir", workdir, "--agents", "codex,agy", "--timeout", "240"])
            with (
                mock.patch.object(node, "execute_job", side_effect=fake_exec),
                mock.patch.object(node, "resolve_node_agents", return_value=("codex", ["codex", "agy"])),
                redirect_stdout(io.StringIO()) as stdout,
            ):
                self.assertEqual(node.run_self_check(args), 0)
                self.assertIn("job timeout=240s (--timeout overrides AGENCT_NODE_TIMEOUT, default 3600s)", stdout.getvalue())
                self.assertIn("probe timeout=180s", stdout.getvalue())

            with (
                mock.patch.object(node, "execute_job", return_value=("failed", "Action denied", [])),
                mock.patch.object(node, "resolve_node_agents", return_value=("agy", ["agy"])),
            ):
                self.assertEqual(node.run_self_check(args), 1)

    def test_codex_tmux_propagates_across_all_paths_and_only_in_workspace_write(self) -> None:
        ok_stream = b'{"type":"item.completed","item":{"type":"agent_message","text":"OK"}}\n'
        with tempfile.TemporaryDirectory() as workdir:
            # 1) Initial + 2) Resume + 3) Resume fallback (fresh reopen) with codex_tmux=True vs False
            for allow_tmux, expected_cfg in (
                (False, "sandbox_workspace_write.network_access=false"),
                (True, "sandbox_workspace_write.network_access=true"),
            ):
                with self.subTest(codex_tmux=allow_tmux):
                    seen_argvs: list[list[str]] = []

                    def fake_run(argv, **_kw):
                        seen_argvs.append(list(argv))
                        if len(seen_argvs) == 2:
                            return (1, b"", b"session expired", False)
                        return (0, ok_stream, b"", False)

                    with (
                        mock.patch.object(node.shutil, "which", return_value="/bin/codex"),
                        mock.patch.object(node, "run_monitored", side_effect=fake_run),
                    ):
                        # Initial execution
                        st1, _, _ = node.execute_job(
                            "codex", {"task_id": "t1", "title": "Init", "instruction": "Hi"},
                            workdir, "workspace-write", 30, threading.Event(), lambda _s, _t: None,
                            codex_tmux=allow_tmux, sandbox_backend="cli-native",
                        )
                        # Resume fails -> falls back to fresh reopen
                        st2, _, _ = node.execute_job(
                            "codex",
                            {"task_id": "t2", "title": "Res", "instruction": "Hi", "agent_session_id": "th_prev"},
                            workdir, "workspace-write", 30, threading.Event(), lambda _s, _t: None,
                            codex_tmux=allow_tmux, sandbox_backend="cli-native",
                        )
                    self.assertEqual((st1, st2), ("completed", "completed"))
                    self.assertEqual(len(seen_argvs), 3)
                    self.assertEqual(seen_argvs[0][:2], ["/bin/codex", "exec"])
                    self.assertNotEqual(seen_argvs[0][2], "resume")
                    self.assertEqual(seen_argvs[1][:3], ["/bin/codex", "exec", "resume"])
                    self.assertEqual(seen_argvs[2][:2], ["/bin/codex", "exec"])
                    self.assertNotEqual(seen_argvs[2][2], "resume")
                    for argv in seen_argvs:
                        self.assertIn(expected_cfg, argv)

            # Per-job readonly tightening ignores codex_tmux=True
            ro_argvs: list[list[str]] = []
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/codex"),
                mock.patch.object(
                    node, "run_monitored",
                    side_effect=lambda argv, **_kw: (ro_argvs.append(list(argv)) or (0, ok_stream, b"", False)),
                ),
            ):
                node.execute_job(
                    "codex", {"task_id": "t3", "title": "RO", "instruction": "Hi", "sandbox": "readonly"},
                    workdir, "workspace-write", 30, threading.Event(), lambda _s, _t: None,
                    codex_tmux=True, sandbox_backend="cli-native",
                )
            self.assertEqual(len(ro_argvs), 1)
            self.assertNotIn("sandbox_workspace_write.network_access=true", ro_argvs[0])

            # 4) self-check path: propagates --codex-tmux to workspace-write probe, never to readonly probe
            sc_argvs: list[list[str]] = []

            def fake_sc_run(argv, **_kw):
                sc_argvs.append(list(argv))
                prompt = argv[-1]
                if "Read " in prompt:
                    path = prompt.split("Read ", 1)[1].split(" and reply", 1)[0]
                    tok = Path(path).read_text(encoding="utf-8").strip()
                else:
                    tok = prompt.split("print ", 1)[1].split(" (e.g.", 1)[0]
                return (0, json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": tok}}).encode() + b"\n", b"", False)

            sc_args = node.parse_args(
                ["--self-check", "--workdir", workdir, "--agents", "codex", "--sandbox-backend", "cli-native", "--codex-tmux"]
            )
            self.assertTrue(sc_args.codex_tmux)
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/codex"),
                mock.patch.object(node, "resolve_node_agents", return_value=("codex", ["codex"])),
                mock.patch.object(node, "run_monitored", side_effect=fake_sc_run),
                redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(node.run_self_check(sc_args), 0)
            self.assertEqual(len(sc_argvs), 2)
            self.assertNotIn("sandbox_workspace_write.network_access=true", sc_argvs[0])
            self.assertIn("sandbox_workspace_write.network_access=true", sc_argvs[1])

            # readonly node with --codex-tmux: ignored with warning (never enables network)
            sc_ro_argvs: list[list[str]] = []
            sc_ro_args = node.parse_args(
                [
                    "--self-check", "--workdir", workdir, "--agents", "codex",
                    "--sandbox", "readonly", "--sandbox-backend", "cli-native", "--codex-tmux",
                ]
            )
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/codex"),
                mock.patch.object(node, "resolve_node_agents", return_value=("codex", ["codex"])),
                mock.patch.object(
                    node, "run_monitored",
                    side_effect=lambda argv, **kw: (sc_ro_argvs.append(list(argv)) or fake_sc_run(argv, **kw)),
                ),
                redirect_stdout(io.StringIO()),
                redirect_stderr(io.StringIO()) as sc_ro_err,
            ):
                self.assertEqual(node.run_self_check(sc_ro_args), 0)
            self.assertIn("--codex-tmux only takes effect under sandbox=workspace-write", sc_ro_err.getvalue())
            self.assertEqual(len(sc_ro_argvs), 1)
            self.assertNotIn("sandbox_workspace_write.network_access=true", sc_ro_argvs[0])

            # run_node_loop forwards --codex-tmux only under workspace-write
            for sb_flag, expected_flag in (("workspace-write", True), ("readonly", False)):
                with self.subTest(loop_sandbox=sb_flag):
                    captured_kw: dict = {}

                    def fake_loop_exec(_agent, _job, _wd, _sb, _to, _ev, _cb, **kw):
                        captured_kw.update(kw)
                        return ("completed", "done", [])

                    loop_args = node.parse_args([
                        "--hub", "https://agenct.example.com", "--token", "tok", "--once",
                        "--agent", "codex", "--agents", "codex", "--workdir", workdir,
                        "--state-dir", workdir, "--sandbox", sb_flag, "--sandbox-backend", "cli-native", "--codex-tmux",
                    ])
                    with (
                        mock.patch.object(node, "execute_job", side_effect=fake_loop_exec),
                        mock.patch.object(
                            node, "http_json",
                            side_effect=lambda url, *_a, **_k: (
                                {"job": {"task_id": "t_loop", "task_revision": 1, "lease_token": "l", "agent": "codex"}}
                                if url.endswith("/work") else {}
                            ),
                        ),
                        redirect_stdout(io.StringIO()),
                        redirect_stderr(io.StringIO()) as loop_err,
                    ):
                        self.assertEqual(node.run_node_loop(loop_args), 0)
                    self.assertEqual(captured_kw.get("codex_tmux"), expected_flag)
                    if not expected_flag:
                        self.assertIn("--codex-tmux only takes effect under sandbox=workspace-write", loop_err.getvalue())

    def test_exec_backend_cli_args_and_codex_tmux_orthogonality(self) -> None:
        base = ["--hub", "https://agenct.example.com"]
        with mock.patch.dict(os.environ, {}, clear=True):
            args_default = node.parse_args(base)
            self.assertEqual(args_default.exec_backend, "auto")
            self.assertFalse(args_default.codex_tmux)

            for choice in ("auto", "tmux", "conpty", "wt-tail", "pipe"):
                parsed = node.parse_args([*base, "--exec-backend", choice])
                self.assertEqual(parsed.exec_backend, choice)
                self.assertFalse(parsed.codex_tmux)

            # Orthogonal to --codex-tmux: neither flag alters the other's semantics
            ortho1 = node.parse_args([*base, "--exec-backend", "tmux"])
            self.assertEqual(ortho1.exec_backend, "tmux")
            self.assertFalse(ortho1.codex_tmux)

            ortho2 = node.parse_args([*base, "--codex-tmux", "--exec-backend", "pipe"])
            self.assertEqual(ortho2.exec_backend, "pipe")
            self.assertTrue(ortho2.codex_tmux)

        with mock.patch.dict(os.environ, {"AGENCT_EXEC_BACKEND": "wt-tail"}, clear=True):
            self.assertEqual(node.parse_args(base).exec_backend, "wt-tail")
        with mock.patch.dict(os.environ, {"AGENCT_EXEC_BACKEND": "bogus"}, clear=True), redirect_stderr(io.StringIO()) as err:
            self.assertEqual(node.parse_args(base).exec_backend, "auto")
            self.assertIn("invalid AGENCT_EXEC_BACKEND", err.getvalue())

    def test_exec_backend_probe_and_auto_selection_all_tiers(self) -> None:
        def ok_run(cmd, **_kw):
            return subprocess.CompletedProcess(cmd, 0, b"tmux 3.4\n", b"")

        # 1) Linux / macOS: tmux available -> tmux; missing -> pipe; unsafe socket -> pipe
        for plat in ("Linux", "Darwin"):
            b_tmux = node.resolve_exec_backend(
                "auto", system=plat, which=lambda c: "/usr/bin/tmux" if c == "tmux" else None, runner=ok_run,
            )
            self.assertIsInstance(b_tmux, node.TmuxBackend)
            self.assertEqual(b_tmux.name, "tmux")

            with redirect_stderr(io.StringIO()) as err:
                b_pipe = node.resolve_exec_backend("auto", system=plat, which=lambda _c: None, runner=ok_run)
            self.assertIsInstance(b_pipe, node.PipeBackend)
            self.assertEqual(b_pipe.name, "pipe")
            self.assertEqual(err.getvalue(), "")

        def unsafe_tmux_run(cmd, **_kw):
            if "-V" in cmd:
                return subprocess.CompletedProcess(cmd, 0, b"tmux 3.2a\n", b"")
            return subprocess.CompletedProcess(cmd, 1, b"", b"directory /tmp/tmux-1000 has unsafe permissions\n")

        b_unsafe = node.resolve_exec_backend(
            "auto", system="Linux", which=lambda c: "/usr/bin/tmux" if c == "tmux" else None, runner=unsafe_tmux_run,
        )
        self.assertEqual(b_unsafe.name, "pipe")

        # 2) WSL2: must be treated strictly as Linux, NEVER invoke /mnt/c/.../wt.exe
        self.assertFalse(node.is_windows("Linux"))
        self.assertTrue(node.is_wsl2("Linux", "6.18.58.1-microsoft-standard-WSL2", {"WSL_DISTRO_NAME": "Ubuntu"}))
        wt_calls: list[list[str]] = []

        def wsl_which(cmd: str) -> str | None:
            if cmd in ("wt.exe", "wt"):
                return "/mnt/c/Users/dev/AppData/Local/Microsoft/WindowsApps/wt.exe"
            return None

        def wsl_runner(cmd, **_kw):
            wt_calls.append(list(cmd))
            return subprocess.CompletedProcess(cmd, 0, b"1.20\n", b"")

        b_wsl_notmux = node.resolve_exec_backend("auto", system="Linux", which=wsl_which, runner=wsl_runner)
        self.assertEqual(b_wsl_notmux.name, "pipe")
        self.assertEqual(wt_calls, [])

        b_wsl_explicit_wt = node.resolve_exec_backend("wt-tail", system="Linux", which=wsl_which, runner=wsl_runner)
        self.assertEqual(b_wsl_explicit_wt.name, "pipe")
        self.assertEqual(wt_calls, [])

        # Even if tmux IS present alongside /mnt/c/wt.exe on WSL2, choose tmux and never touch wt.exe
        def wsl_which_with_tmux(cmd: str) -> str | None:
            if cmd == "tmux":
                return "/usr/bin/tmux"
            return wsl_which(cmd)

        b_wsl_tmux = node.resolve_exec_backend("auto", system="Linux", which=wsl_which_with_tmux, runner=ok_run)
        self.assertEqual(b_wsl_tmux.name, "tmux")

        # 3) Windows: conpty (pywinpty) -> conpty (node-pty) -> wt-tail -> pipe
        fake_winpty = type("FakeWinpty", (), {"PtyProcess": object})()
        b_win_pywinpty = node.resolve_exec_backend(
            "auto",
            system="Windows",
            which=lambda _c: None,
            import_module=lambda name: fake_winpty if name == "winpty" else (_ for _ in ()).throw(ImportError(name)),
            runner=ok_run,
        )
        self.assertIsInstance(b_win_pywinpty, node.ConPtyBackend)
        self.assertEqual(b_win_pywinpty.name, "conpty")
        self.assertEqual(b_win_pywinpty.provider, "pywinpty")

        def fail_winpty(_name: str):
            raise ImportError("No module named 'winpty'")

        # pywinpty missing, node-pty available -> conpty (node-pty)
        b_win_nodepty = node.resolve_exec_backend(
            "auto",
            system="Windows",
            which=lambda c: r"C:\Program Files\nodejs\node.exe" if c == "node" else None,
            import_module=fail_winpty,
            runner=lambda cmd, **_kw: subprocess.CompletedProcess(cmd, 0, b"", b""),
        )
        self.assertIsInstance(b_win_nodepty, node.ConPtyBackend)
        self.assertEqual(b_win_nodepty.provider, "node-pty")

        # pywinpty & node-pty missing, wt.exe working -> wt-tail
        b_win_wt = node.resolve_exec_backend(
            "auto",
            system="Windows",
            which=lambda c: r"C:\Users\dev\AppData\Local\Microsoft\WindowsApps\wt.exe" if c == "wt.exe" else None,
            import_module=fail_winpty,
            runner=lambda cmd, **_kw: subprocess.CompletedProcess(cmd, 0, b"1.20.0\n", b""),
        )
        self.assertIsInstance(b_win_wt, node.WtTailBackend)
        self.assertEqual(b_win_wt.name, "wt-tail")

        # pywinpty missing & wt.exe missing -> silent degradation to pipe
        with redirect_stderr(io.StringIO()) as err:
            b_win_missing_wt = node.resolve_exec_backend(
                "auto", system="Windows", which=lambda _c: None, import_module=fail_winpty, runner=ok_run,
            )
        self.assertIsInstance(b_win_missing_wt, node.PipeBackend)
        self.assertEqual(err.getvalue(), "")

        # wt.exe Store App Execution Alias broken (WinError 1920 / OSError) -> silent degradation to pipe
        def broken_store_alias(_cmd, **_kw):
            raise OSError(1920, "The file cannot be accessed by the system")

        with redirect_stderr(io.StringIO()) as err:
            b_win_broken_alias = node.resolve_exec_backend(
                "auto",
                system="Windows",
                which=lambda c: r"C:\Users\dev\AppData\Local\Microsoft\WindowsApps\wt.exe" if c == "wt.exe" else None,
                import_module=fail_winpty,
                runner=broken_store_alias,
            )
        self.assertIsInstance(b_win_broken_alias, node.PipeBackend)
        self.assertEqual(err.getvalue(), "")

        # Explicit conpty when pywinpty raises ImportError -> safe fallback to pipe without crashing
        b_explicit_conpty = node.resolve_exec_backend(
            "conpty", system="Windows", which=lambda _c: None, import_module=fail_winpty,
        )
        self.assertIsInstance(b_explicit_conpty, node.PipeBackend)

    def test_runs_dir_windows_uses_userprofile_and_linux_keeps_tmp(self) -> None:
        win_dir = node.get_runs_dir(system="Windows", env={"USERPROFILE": r"C:\Users\alice"})
        self.assertEqual(win_dir, r"C:\Users\alice\.agenct\runs")
        self.assertNotIn("/tmp", win_dir)

        linux_dir = node.get_runs_dir(system="Linux", env={})
        self.assertEqual(linux_dir, os.path.join(tempfile.gettempdir(), "agenct-runs"))

    def test_wt_tail_whitelisted_argv_and_runtime_degradation(self) -> None:
        wt_argv = node.build_wt_tail_argv("wt.exe", "tsk_42", r"C:\Users\alice\.agenct\runs\tsk_42.log")
        self.assertEqual(
            wt_argv,
            [
                "wt.exe", "-w", "0", "new-tab",
                "--title", "agenct:tsk_42",
                "pwsh", "-NoExit", "-Command",
                r"Get-Content -Wait C:\Users\alice\.agenct\runs\tsk_42.log",
            ],
        )

        # Simulated wt-tail execution with mocked wt.exe launch
        with tempfile.TemporaryDirectory() as tmp:
            runs_dir = os.path.join(tmp, "runs")
            real_popen = subprocess.Popen
            wt_launched: list[list[str]] = []

            def selective_popen(cmd, *a, **kw):
                if isinstance(cmd, list) and cmd and cmd[0] == "wt.exe":
                    wt_launched.append(list(cmd))
                    return mock.MagicMock()
                return real_popen(cmd, *a, **kw)

            backend = node.WtTailBackend(wt_bin="wt.exe", runs_dir=runs_dir)
            meta: dict = {}
            with mock.patch.object(node.subprocess, "Popen", side_effect=selective_popen):
                rc, out, err, cancelled = node.run_monitored(
                    [sys.executable, "-c", "import sys; print('wt-ok'); print('wt-err', file=sys.stderr)"],
                    shell=False, cwd=tmp, env=dict(os.environ), timeout=10,
                    cancel_event=threading.Event(), progress_cb=lambda _s, _t: None,
                    stage_label="test", backend=backend, task_id="tsk_wt", meta_out=meta,
                )
            self.assertEqual(rc, 0)
            self.assertFalse(cancelled)
            self.assertEqual(out.decode().strip(), "wt-ok")
            self.assertEqual(err.decode().strip(), "wt-err")
            self.assertEqual(meta.get("backend"), "wt-tail")
            self.assertEqual(len(wt_launched), 1)
            self.assertEqual(wt_launched[0][:6], ["wt.exe", "-w", "0", "new-tab", "--title", "agenct:tsk_wt"])
            self.assertIn("Get-Content -Wait", wt_launched[0][-1])
            self.assertEqual(backend.list(), [])

            # If wt.exe fails at spawn time (e.g. broken Store alias), run_monitored silently degrades to pipe
            def failing_wt_popen(cmd, *a, **kw):
                if isinstance(cmd, list) and cmd and cmd[0] == "wt.exe":
                    raise OSError(1920, "Store alias unavailable")
                return real_popen(cmd, *a, **kw)

            meta_deg: dict = {}
            with mock.patch.object(node.subprocess, "Popen", side_effect=failing_wt_popen):
                rc2, out2, _err2, _c2 = node.run_monitored(
                    [sys.executable, "-c", "print('fallback-pipe')"],
                    shell=False, cwd=tmp, env=dict(os.environ), timeout=10,
                    cancel_event=threading.Event(), progress_cb=lambda _s, _t: None,
                    stage_label="test", backend=backend, task_id="tsk_deg", meta_out=meta_deg,
                )
            self.assertEqual(rc2, 0)
            self.assertEqual(out2.decode().strip(), "fallback-pipe")
            self.assertEqual(meta_deg.get("backend"), "pipe")

    def test_conpty_wrapper_preserves_long_jsonl_and_import_failure_falls_back(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            runs_dir = os.path.join(tmp, "runs")

            class FakePtyProcess:
                def __init__(self, proc: subprocess.Popen) -> None:
                    self._proc = proc

                @classmethod
                def spawn(cls, cmd, cwd=None):
                    p = subprocess.Popen(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                    return cls(p)

                def read(self):
                    return self._proc.stdout.read(1024) if self._proc.stdout else b""

                def isalive(self):
                    return self._proc.poll() is None

                def terminate(self, force=False):
                    if self._proc.poll() is None:
                        self._proc.kill() if force else self._proc.terminate()

                def close(self):
                    try:
                        self._proc.wait(timeout=2)
                    finally:
                        for st in (self._proc.stdout, self._proc.stderr):
                            if st is not None:
                                st.close()

            fake_winpty = type("FakeWinpty", (), {"PtyProcess": FakePtyProcess})()
            long_text = "X" * 2400
            emit_script = (
                "import json; "
                f"print(json.dumps({{'type':'result','result':'{long_text}'}}), flush=True)"
            )
            backend = node.ConPtyBackend(
                provider="pywinpty",
                runs_dir=runs_dir,
                import_module=lambda name: fake_winpty if name == "winpty" else (_ for _ in ()).throw(ImportError(name)),
            )
            meta: dict = {}
            rc, out, _err, cancelled = node.run_monitored(
                [sys.executable, "-c", emit_script],
                shell=False, cwd=tmp, env=dict(os.environ), timeout=10,
                cancel_event=threading.Event(), progress_cb=lambda _s, _t: None,
                stage_label="test", backend=backend, task_id="tsk_conpty", meta_out=meta,
            )
            self.assertEqual(rc, 0)
            self.assertFalse(cancelled)
            self.assertEqual(meta.get("backend"), "conpty")
            lines = out.decode("utf-8").splitlines()
            self.assertEqual(len(lines), 1)
            self.assertEqual(json.loads(lines[0])["result"], long_text)
            self.assertEqual(backend.list(), [])

            # Import failure during spawn degrades safely to pipe without crashing
            broken_backend = node.ConPtyBackend(
                provider="pywinpty",
                runs_dir=runs_dir,
                import_module=lambda _n: (_ for _ in ()).throw(ImportError("winpty missing")),
            )
            meta_fb: dict = {}
            rc_fb, out_fb, _, _ = node.run_monitored(
                [sys.executable, "-c", "print('safe-fallback')"],
                shell=False, cwd=tmp, env=dict(os.environ), timeout=10,
                cancel_event=threading.Event(), progress_cb=lambda _s, _t: None,
                stage_label="test", backend=broken_backend, task_id="tsk_fb", meta_out=meta_fb,
            )
            self.assertEqual(rc_fb, 0)
            self.assertEqual(out_fb.decode().strip(), "safe-fallback")
            self.assertEqual(meta_fb.get("backend"), "pipe")

    def test_tmux_backend_real_isolated_socket_and_interface(self) -> None:
        if node.is_windows() or not node.shutil.which("tmux"):
            self.skipTest("tmux not available on this host")
        with tempfile.TemporaryDirectory() as tmp:
            os.chmod(tmp, 0o700)
            sock = os.path.join(tmp, "tmux.sock")
            runs_dir = os.path.join(tmp, "runs")
            if not node.probe_tmux(socket_path=sock):
                self.skipTest("tmux socket unavailable in this environment")
            backend = node.TmuxBackend(runs_dir=runs_dir, socket_path=sock)
            try:
                long_payload = "Y" * 2200
                script = (
                    "import json, sys, time; "
                    f"print(json.dumps({{'type':'result','result':'{long_payload}'}}), flush=True); "
                    "print('stderr-line', file=sys.stderr, flush=True); "
                    "time.sleep(0.2)"
                )
                meta: dict = {}
                seen_sessions: list[list[dict[str, str]]] = []

                def on_prog(_stage, _tail):
                    if not seen_sessions:
                        seen_sessions.append(backend.list())

                rc, out, err, cancelled = node.run_monitored(
                    [sys.executable, "-c", script],
                    shell=False, cwd=tmp, env=dict(os.environ), timeout=10,
                    cancel_event=threading.Event(), progress_cb=on_prog,
                    stage_label="tmux-test", backend=backend, task_id="tsk_tmux", meta_out=meta,
                )
                self.assertEqual(rc, 0)
                self.assertFalse(cancelled)
                self.assertEqual(meta.get("backend"), "tmux")
                self.assertTrue(str(meta.get("tmux_session", "")).startswith("agenct-tsk_tmux-"))
                self.assertEqual(meta.get("attach_cmd")[-2:], ["-t", meta["tmux_session"]])
                self.assertEqual(json.loads(out.decode("utf-8").strip())["result"], long_payload)
                self.assertEqual(err.decode("utf-8").strip(), "stderr-line")
                self.assertEqual(backend.list(), [])
            finally:
                subprocess.run(["tmux", "-S", sock, "kill-server"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    def test_run_monitored_depends_only_on_backend_interface(self) -> None:
        events: list[str] = []

        class StubLog(node.BackendLogStream):
            def __init__(self) -> None:
                self._step = 0

            def read_chunks(self) -> tuple[bytes, bytes]:
                if self._step == 0:
                    self._step = 1
                    return b"step-1\n", b""
                return b"", b""

            def is_done(self) -> bool:
                return self._step >= 1

            def returncode(self) -> int | None:
                return 0 if self._step >= 1 else None

        class StubBackend(node.ExecBackend):
            name = "stub"

            def spawn(self, argv, *, shell, cwd, env, task_id=""):
                events.append("spawn")
                return node.ExecHandle(backend=self.name, session_id="stub-1", argv=argv)

            def log(self, handle):
                events.append("log")
                return StubLog()

            def attach(self, handle):
                events.append("attach")
                return ["stub-attach", handle.session_id]

            def cleanup(self, handle, *, terminate=False, force=False):
                events.append(f"cleanup:{terminate}:{force}")

            def list(self):
                return [{"backend": self.name, "session_id": "stub-1"}]

        meta: dict = {}
        progress: list[tuple[str, str]] = []
        rc, out, err, cancelled = node.run_monitored(
            ["noop"], shell=False, cwd=".", env={}, timeout=5,
            cancel_event=threading.Event(),
            progress_cb=lambda st, tl: progress.append((st, tl)),
            stage_label="Stub", backend=StubBackend(), task_id="t1", meta_out=meta,
        )
        self.assertEqual((rc, out, err, cancelled), (0, b"step-1\n", b"", False))
        self.assertEqual(meta["backend"], "stub")
        self.assertEqual(meta["attach_cmd"], ["stub-attach", "stub-1"])
        self.assertEqual(events, ["spawn", "attach", "log", "cleanup:False:False"])
        self.assertEqual(progress, [("Stub", "step-1")])

    def test_default_auto_behavior_and_backend_reported_in_task_receipt(self) -> None:
        for resolved_obj, expected_backend in (
            (node.TmuxBackend(tmux_bin="/usr/bin/tmux"), "tmux"),
            (node.PipeBackend(), "pipe"),
        ):
            with self.subTest(expected_backend=expected_backend), tempfile.TemporaryDirectory() as workdir:
                submitted_payloads: list[dict] = []

                def fake_http(url, _token, *, method, **kwargs):
                    if url.endswith("/work"):
                        return {"job": {"task_id": "tsk_be", "task_revision": 1, "lease_token": "l", "agent": ""}}
                    if url.endswith("/submit"):
                        self.assertEqual(method, "POST")
                        submitted_payloads.append(kwargs["body"])
                        return {}
                    return {}

                # Default CLI args (no --exec-backend passed -> auto)
                args = node.parse_args([
                    "--hub", "https://agenct.example.com", "--token", "tok", "--once",
                    "--cmd", "python3 -c \"print('ok')\"",
                    "--workdir", workdir, "--state-dir", workdir,
                ])
                self.assertEqual(args.exec_backend, "auto")
                with (
                    mock.patch.object(node, "resolve_exec_backend", return_value=resolved_obj),
                    mock.patch.object(node, "http_json", side_effect=fake_http),
                    redirect_stdout(io.StringIO()),
                ):
                    self.assertEqual(node.run_node_loop(args), 0)
                self.assertEqual(len(submitted_payloads), 1)
                self.assertEqual(submitted_payloads[0].get("backend"), expected_backend)
                self.assertEqual(submitted_payloads[0].get("status"), "completed")

    def test_resolve_job_sandbox_only_elevates_manual_when_opted_in(self) -> None:
        self.assertEqual(
            node.resolve_job_sandbox("workspace-write", {"sandbox": "elevated", "authorization": "manual"}, allow_elevated=False),
            "workspace-write",
        )
        self.assertEqual(
            node.resolve_job_sandbox("workspace-write", {"sandbox": "elevated", "authorization": "cron"}, allow_elevated=True),
            "workspace-write",
        )
        self.assertEqual(
            node.resolve_job_sandbox("workspace-write", {"sandbox": "elevated", "authorization": "delegation"}, allow_elevated=True),
            "workspace-write",
        )
        self.assertEqual(
            node.resolve_job_sandbox("workspace-write", {"sandbox": "elevated", "authorization": "manual"}, allow_elevated=True),
            "unsafe-full",
        )
        self.assertEqual(
            node.resolve_job_sandbox("workspace-write", {"sandbox": "readonly", "authorization": "manual"}, allow_elevated=True),
            "readonly",
        )

    def test_detect_sandbox_block_extracts_reasons_and_populates_meta_out(self) -> None:
        oc_err = "! permission requested: external_directory (/proc/*); auto-rejecting\n"
        self.assertIn(
            "external_directory (/proc/*)",
            node.detect_sandbox_block("opencode", '{"type":"step_start"}\n', oc_err),
        )
        self.assertIn(
            "Action denied in headless mode: Bash",
            node.detect_sandbox_block("claude", "", "", "Partial\nAction denied in headless mode: Bash"),
        )
        self.assertEqual(node.detect_sandbox_block("codex", "All good", "", "Done"), "")

        with tempfile.TemporaryDirectory() as workdir:
            meta: dict = {}
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/opencode"),
                mock.patch.object(
                    node,
                    "run_monitored",
                    return_value=(1, b"", oc_err.encode("utf-8"), False),
                ),
            ):
                st, _, _ = node.execute_job(
                    "opencode",
                    {"task_id": "tsk_blk", "title": "Check", "instruction": "Inspect /proc"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    meta_out=meta,
                )
            self.assertEqual(st, "failed")
            self.assertIn("external_directory (/proc/*)", meta.get("blocked_reason", ""))

    def test_exit0_prose_mentioning_sandbox_or_ssh_errors_stays_completed(self) -> None:
        raw_prose_with_err = (
            '{"event":"step_update","step_update":{"step_type":"tool","tool_output":"OUT_ERR:No such file or directory"}}\n'
            '{"event":"step_update","step_update":{"step_type":"agent_response","text_delta":"单测说明：预期 `Read-only file system`，实际 `OUT_ERR:No such file or directory`；远端推送因沙箱隔离 `~/.ssh`（Permission denied (publickey)）按要求跳过。"}}\n'
        ).encode("utf-8")
        with tempfile.TemporaryDirectory() as workdir:
            meta: dict = {}
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/agy"),
                mock.patch.object(node, "run_monitored", return_value=(0, raw_prose_with_err, b"", False)),
            ):
                st, _, _ = node.execute_job(
                    "agy",
                    {"task_id": "tsk_ok", "title": "Verify", "instruction": "run tests", "sandbox": "workspace-write"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    allow_elevated=True,
                    sandbox_backend="cli-native",
                    meta_out=meta,
                )
            self.assertEqual(st, "completed")
            self.assertEqual(meta.get("blocked_reason", ""), "")

            # Structured CLI denial events in stream still deterministically fail with clean blocked_reason.
            raw_structured_denial = (
                '{"event":"result","result":{"status":"SUCCESS","response":"无法执行","denied_actions":[{"tool":"bash","reason":"`Read-only file system`）。"}]}}\n'
            ).encode("utf-8")
            meta_denied: dict = {}
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/agy"),
                mock.patch.object(node, "run_monitored", return_value=(0, raw_structured_denial, b"", False)),
            ):
                st_d, _, _ = node.execute_job(
                    "agy",
                    {"task_id": "tsk_den", "title": "Denied", "instruction": "write /etc"},
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    sandbox_backend="cli-native",
                    meta_out=meta_denied,
                )
            self.assertEqual(st_d, "failed")
            self.assertNotIn("`", meta_denied.get("blocked_reason", ""))
            self.assertIn("Action denied", meta_denied.get("blocked_reason", ""))

    def test_build_env_header_includes_sandbox_and_elevated_flags(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            os.makedirs(os.path.join(tmp, "repo-a", ".git"))
            hdr = node.build_env_header(
                "pc", "codex", tmp, ["codex", "agy"],
                sandbox="workspace-write", isolation="cli-native", allow_elevated=True, codex_tmux=True,
            )
            self.assertIn("sandbox:workspace-write", hdr)
            self.assertIn("isolation:cli-native", hdr)
            self.assertIn("elevated:1", hdr)
            self.assertIn("codex_net:1", hdr)
            self.assertLess(hdr.index("sandbox:"), hdr.index("repos:"))

            hdr_bwrap = node.build_env_header(
                "pc", "codex", tmp, ["codex", "agy"],
                sandbox="workspace-write", isolation="bwrap", allow_elevated=True, codex_tmux=True,
            )
            self.assertIn("isolation:bwrap", hdr_bwrap)
            self.assertNotIn("codex_net:1", hdr_bwrap)

            hdr_direct = node.build_env_header(
                "pc", "opencode", tmp, ["opencode"],
                sandbox="unsafe-full", isolation="bwrap",
            )
            self.assertIn("sandbox:unsafe-full", hdr_direct)
            self.assertIn("isolation:direct", hdr_direct)

    def test_resolve_sandbox_backend_auto_and_explicit_fallback(self) -> None:
        ok_proc = subprocess.CompletedProcess([], 0)
        fail_proc = subprocess.CompletedProcess([], 1)

        sb_linux = node.resolve_sandbox_backend(
            "auto",
            system="Linux",
            which=lambda c: "/usr/bin/bwrap" if c == "bwrap" else None,
            runner=lambda *_a, **_k: ok_proc,
        )
        self.assertEqual(sb_linux.name, "bwrap")
        self.assertTrue(sb_linux.os_isolated)

        sb_linux_fail = node.resolve_sandbox_backend(
            "auto",
            system="Linux",
            which=lambda c: "/usr/bin/bwrap" if c == "bwrap" else None,
            runner=lambda *_a, **_k: fail_proc,
        )
        self.assertEqual(sb_linux_fail.name, "cli-native")
        self.assertFalse(sb_linux_fail.os_isolated)

        sb_darwin = node.resolve_sandbox_backend(
            "auto",
            system="Darwin",
            which=lambda c: "/usr/bin/sandbox-exec" if c == "sandbox-exec" else None,
            runner=lambda *_a, **_k: ok_proc,
        )
        self.assertEqual(sb_darwin.name, "seatbelt")
        self.assertTrue(sb_darwin.os_isolated)

        sb_win = node.resolve_sandbox_backend("auto", system="Windows", which=lambda _c: "/bin/bwrap")
        self.assertEqual(sb_win.name, "cli-native")

        with redirect_stderr(io.StringIO()) as err_buf:
            sb_explicit_fail = node.resolve_sandbox_backend(
                "bwrap",
                system="Linux",
                which=lambda _c: None,
            )
        self.assertEqual(sb_explicit_fail.name, "cli-native")
        self.assertIn("falling back", err_buf.getvalue())

        args_default = node.parse_args(["--self-check"])
        self.assertEqual(args_default.sandbox_backend, "auto")
        with mock.patch.dict(os.environ, {node.SANDBOX_BACKEND_ENV: "cli-native"}):
            self.assertEqual(node.parse_args(["--self-check"]).sandbox_backend, "cli-native")
            self.assertEqual(
                node.parse_args(["--self-check", "--sandbox-backend", "bwrap"]).sandbox_backend,
                "bwrap",
            )

    def test_bubblewrap_sandbox_mounts_order_secret_masking_and_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as base:
            workdir = os.path.join(base, "work")
            secret_dir = os.path.join(base, ".ssh")
            secret_file = os.path.join(base, ".netrc")
            broken_link = os.path.join(base, ".aws")
            ev_dir = os.path.join(base, "ev")
            in_dir = os.path.join(base, "in")
            out_file = os.path.join(base, "out.txt")
            state_dir = os.path.join(base, ".opencode-state")
            for d in (workdir, secret_dir, ev_dir, in_dir, state_dir):
                os.makedirs(d, exist_ok=True)
            for f in (secret_file, out_file):
                with open(f, "w", encoding="utf-8") as fh:
                    fh.write("x")
            os.symlink(os.path.join(base, "does-not-exist"), broken_link)

            bw = node.BubblewrapSandbox(bwrap_bin="/usr/bin/bwrap")
            policy_ro = node.SandboxPolicy(
                mode="readonly",
                workdir=workdir,
                evidence_dir=ev_dir,
                in_dir=in_dir,
                out_file=out_file,
                state_paths=(state_dir, os.path.join(base, "missing-state")),
                masked_paths=(secret_dir, secret_file, broken_link, workdir),
            )
            argv_ro, shell_ro, cwd_ro, _ = bw.wrap(["/bin/opencode", "run"], shell=False, cwd=workdir, env={"A": "1"}, policy=policy_ro)
            self.assertFalse(shell_ro)
            self.assertEqual(cwd_ro, workdir)
            joined = " ".join(argv_ro)
            self.assertIn(f"--tmpfs {os.path.realpath(secret_dir)}", joined)
            self.assertIn(f"--ro-bind /dev/null {os.path.realpath(secret_file)}", joined)
            self.assertNotIn(os.path.realpath(broken_link), joined)
            self.assertNotIn(f"--tmpfs {os.path.realpath(workdir)}", joined)
            self.assertIn(f"--ro-bind {os.path.realpath(workdir)} {os.path.realpath(workdir)}", joined)
            self.assertLess(argv_ro.index("/tmp"), argv_ro.index(os.path.realpath(ev_dir)))
            self.assertIn(f"--ro-bind {os.path.realpath(in_dir)} {os.path.realpath(in_dir)}", joined)
            self.assertIn(f"--bind {os.path.realpath(out_file)} {os.path.realpath(out_file)}", joined)
            self.assertIn(f"--bind {os.path.realpath(state_dir)} {os.path.realpath(state_dir)}", joined)
            self.assertEqual(argv_ro[-4:], ["--", "/bin/opencode", "run"] if False else ["--chdir", workdir, "--", "/bin/opencode", "run"][-4:])

            policy_wr = node.SandboxPolicy(
                mode="workspace-write",
                workdir=workdir,
                evidence_dir=ev_dir,
                masked_paths=(secret_dir,),
            )
            argv_custom, shell_custom, _, _ = bw.wrap("echo hi", shell=True, cwd=workdir, env={}, policy=policy_wr)
            self.assertFalse(shell_custom)
            self.assertIn(f"--bind {os.path.realpath(workdir)} {os.path.realpath(workdir)}", " ".join(argv_custom))
            self.assertEqual(argv_custom[-3:], ["/bin/sh", "-c", "echo hi"])

            policy_full = node.SandboxPolicy(mode="unsafe-full", workdir=workdir)
            raw_argv, raw_shell, _, _ = bw.wrap(["/bin/true"], shell=False, cwd=workdir, env={}, policy=policy_full)
            self.assertEqual(raw_argv, ["/bin/true"])
            self.assertFalse(raw_shell)

    def test_seatbelt_sandbox_generates_sbpl_profile(self) -> None:
        with tempfile.TemporaryDirectory() as base:
            workdir = os.path.join(base, "work")
            ev_dir = os.path.join(base, "ev")
            secret_dir = os.path.join(base, ".ssh")
            state_file = os.path.join(base, ".claude.json")
            for d in (workdir, ev_dir, secret_dir):
                os.makedirs(d, exist_ok=True)
            with open(state_file, "w", encoding="utf-8") as fh:
                fh.write("{}")

            sb = node.SeatbeltSandbox(sandbox_exec_bin="/usr/bin/sandbox-exec")
            ro_policy = node.SandboxPolicy(
                mode="readonly",
                workdir=workdir,
                evidence_dir=ev_dir,
                state_paths=(state_file,),
                masked_paths=(secret_dir,),
            )
            ro_profile = sb.build_profile(ro_policy)
            self.assertIn("(deny file-write*)", ro_profile)
            self.assertNotIn(f'(allow file-write* (subpath "{workdir}"))', ro_profile)
            self.assertIn(f'(allow file-write* (literal "{os.path.realpath(state_file)}"))', ro_profile)
            self.assertIn(f'(deny file-read* file-write* (subpath "{os.path.realpath(secret_dir)}"))', ro_profile)

            wr_policy = node.SandboxPolicy(
                mode="workspace-write",
                workdir=workdir,
                evidence_dir=ev_dir,
                masked_paths=(secret_dir,),
            )
            wr_profile = sb.build_profile(wr_policy)
            self.assertIn(f'(allow file-write* (subpath "{os.path.realpath(workdir)}"))', wr_profile)
            cmd, is_shell, _, _ = sb.wrap("ls -la", shell=True, cwd=workdir, env={}, policy=wr_policy)
            self.assertFalse(is_shell)
            self.assertEqual(cmd[:2], ["/usr/bin/sandbox-exec", "-p"])
            self.assertEqual(cmd[-3:], ["/bin/sh", "-c", "ls -la"])

    def test_real_bwrap_enforces_readonly_workspace_write_and_secret_masking(self) -> None:
        if not node.probe_bwrap():
            self.skipTest("bubblewrap unavailable in this environment")
        bw = node.BubblewrapSandbox()
        test_parent = os.path.expanduser("~")
        if not os.path.isdir(test_parent) or not os.access(test_parent, os.W_OK):
            test_parent = tempfile.gettempdir()
        with (
            tempfile.TemporaryDirectory() as workdir,
            tempfile.TemporaryDirectory(prefix=".agenct-out-", dir=test_parent) as outside_dir,
            tempfile.TemporaryDirectory(prefix=".agenct-sec-", dir=test_parent) as secret_dir,
            tempfile.TemporaryDirectory() as ev_dir,
        ):
            with open(os.path.join(workdir, "hello.txt"), "w", encoding="utf-8") as fh:
                fh.write("hello-bwrap")
            with open(os.path.join(secret_dir, "id_ed25519"), "w", encoding="utf-8") as fh:
                fh.write("SECRET_KEY")

            ro_policy = node.SandboxPolicy(
                mode="readonly",
                workdir=workdir,
                evidence_dir=ev_dir,
                masked_paths=(secret_dir,),
            )
            probe_py = (
                "import os, pathlib, sys\n"
                "assert pathlib.Path(sys.argv[1], 'hello.txt').read_text() == 'hello-bwrap'\n"
                "pathlib.Path(sys.argv[2], 'ev.txt').write_text('from-ro')\n"
                "assert not os.path.exists(os.path.join(sys.argv[3], 'id_ed25519'))\n"
                "try:\n"
                "  open(os.path.join(sys.argv[1], 'write_fail'), 'w')\n"
                "  raise AssertionError('readonly write unexpectedly succeeded')\n"
                "except OSError as e:\n"
                "  print(f'RO_ERR:{e.strerror}')\n"
            )
            cmd_ro, _, cwd_ro, env_ro = bw.wrap(
                [sys.executable, "-c", probe_py, workdir, ev_dir, secret_dir],
                shell=False, cwd=workdir, env=dict(os.environ), policy=ro_policy,
            )
            res_ro = subprocess.run(cmd_ro, cwd=cwd_ro, env=env_ro, capture_output=True, text=True, timeout=15)
            self.assertEqual(res_ro.returncode, 0, msg=res_ro.stderr)
            self.assertIn("RO_ERR:Read-only file system", res_ro.stdout)
            with open(os.path.join(ev_dir, "ev.txt"), "r", encoding="utf-8") as fh:
                self.assertEqual(fh.read(), "from-ro")

            wr_policy = node.SandboxPolicy(
                mode="workspace-write",
                workdir=workdir,
                evidence_dir=ev_dir,
                masked_paths=(secret_dir,),
            )
            probe_wr_py = (
                "import os, pathlib, sys\n"
                "pathlib.Path(sys.argv[1], 'ok.txt').write_text('wr-ok')\n"
                "assert not os.path.exists(os.path.join(sys.argv[3], 'id_ed25519'))\n"
                "try:\n"
                "  open(os.path.join(sys.argv[2], 'escape_fail'), 'w')\n"
                "  raise AssertionError('outside write unexpectedly succeeded')\n"
                "except OSError as e:\n"
                "  print(f'OUT_ERR:{e.strerror}')\n"
            )
            cmd_wr, _, cwd_wr, env_wr = bw.wrap(
                [sys.executable, "-c", probe_wr_py, workdir, outside_dir, secret_dir],
                shell=False, cwd=workdir, env=dict(os.environ), policy=wr_policy,
            )
            res_wr = subprocess.run(cmd_wr, cwd=cwd_wr, env=env_wr, capture_output=True, text=True, timeout=15)
            self.assertEqual(res_wr.returncode, 0, msg=res_wr.stderr)
            self.assertRegex(
                res_wr.stdout,
                r"OUT_ERR:(?:Read-only file system|No such file or directory)",
            )
            with open(os.path.join(workdir, "ok.txt"), "r", encoding="utf-8") as fh:
                self.assertEqual(fh.read(), "wr-ok")
            self.assertFalse(os.path.exists(os.path.join(outside_dir, "escape_fail")))

    def test_unsafe_full_node_still_uses_os_sandbox_when_job_tightens_readonly(self) -> None:
        if not node.probe_bwrap():
            self.skipTest("bubblewrap unavailable in this environment")
        captured: list[tuple[list[str], str]] = []

        def fake_run(argv, **kwargs):
            captured.append((list(argv), kwargs["cwd"]))
            return (0, b'{"type":"text","part":{"text":"OK"}}\n', b"", False)

        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", side_effect=lambda c: "/usr/bin/bwrap" if c == "bwrap" else "/bin/opencode"),
                mock.patch.object(node, "run_monitored", side_effect=fake_run),
            ):
                os_sb = node.resolve_sandbox_backend("auto")
                self.assertTrue(os_sb.os_isolated)
                meta_full: dict = {}
                st1, _, _ = node.execute_job(
                    "opencode",
                    {"task_id": "t_full", "title": "Full", "instruction": "Run"},
                    workdir,
                    "unsafe-full",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    meta_out=meta_full,
                    sandbox_backend=os_sb,
                )
                self.assertEqual(st1, "completed")
                self.assertEqual(meta_full.get("isolation"), "direct")
                self.assertNotIn("/usr/bin/bwrap", captured[0][0][0])

                meta_ro: dict = {}
                st2, _, _ = node.execute_job(
                    "opencode",
                    {"task_id": "t_ro", "title": "RO", "instruction": "Check", "sandbox": "readonly"},
                    workdir,
                    "unsafe-full",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                    meta_out=meta_ro,
                    sandbox_backend=os_sb,
                )
                self.assertEqual(st2, "completed")
                self.assertEqual(meta_ro.get("isolation"), "bwrap")
                self.assertEqual(captured[1][0][0], "/usr/bin/bwrap")

    def test_opencode_rejects_model_without_provider_prefix_cleanly(self) -> None:
        with tempfile.TemporaryDirectory() as workdir:
            with (
                mock.patch.object(node.shutil, "which", return_value="/bin/opencode"),
                mock.patch.object(node, "run_monitored") as run_mock,
            ):
                status, content, _ = node.execute_job(
                    "opencode",
                    {
                        "task_id": "t_bad_mod",
                        "title": "Check",
                        "instruction": "Run",
                        "model": "gemini-4-neon-high",
                    },
                    workdir,
                    "workspace-write",
                    30,
                    threading.Event(),
                    lambda _s, _t: None,
                )
        self.assertEqual(status, "failed")
        self.assertIn("provider/model", content)
        run_mock.assert_not_called()

    def test_security_env_scrubbing_evidence_symlink_and_no_redirect(self) -> None:
        from http.server import BaseHTTPRequestHandler, HTTPServer
        import urllib.error

        # 1. Sensitive Hub/Node tokens and master keys are scrubbed from child process env.
        with tempfile.TemporaryDirectory() as workdir, mock.patch.dict(
            os.environ,
            {
                "AGENCT_NODE_TOKEN": "agn_secret_node_token_1234",
                "AGENCT_MASTER_KEY": "secret_master_key_0123456789abcdef",
                "AGENCT_GITLAB_CLIENT_SECRET": "secret_gl_client",
            },
        ):
            status, out, _ = node.execute_job(
                'custom:python3 -c "import os; print(sorted(k for k in os.environ if k.startswith(\'AGENCT_\')))"',
                {"task_id": "t_env", "title": "Env", "instruction": "Check"},
                workdir,
                "workspace-write",
                15,
                threading.Event(),
                lambda _s, _t: None,
            )
            self.assertEqual(status, "completed")
            self.assertNotIn("AGENCT_NODE_TOKEN", out)
            self.assertNotIn("AGENCT_MASTER_KEY", out)
            self.assertNotIn("AGENCT_GITLAB_CLIENT_SECRET", out)
            self.assertIn("AGENCT_WORKDIR", out)

        # 2. collect_evidence skips symlinks pointing outside evidence_dir.
        with tempfile.TemporaryDirectory() as outer, tempfile.TemporaryDirectory() as ev_dir:
            secret_path = os.path.join(outer, "secret.txt")
            with open(secret_path, "w", encoding="utf-8") as fh:
                fh.write("TOP_SECRET")
            ok_path = os.path.join(ev_dir, "ok.txt")
            with open(ok_path, "w", encoding="utf-8") as fh:
                fh.write("SAFE_DATA")
            os.symlink(secret_path, os.path.join(ev_dir, "leak.txt"))
            items = node.collect_evidence(ev_dir)
            self.assertEqual([i["name"] for i in items], ["ok.txt"])
            self.assertEqual(base64.b64decode(items[0]["data_base64"]), b"SAFE_DATA")

        # 3. http_json refuses HTTP 3xx redirects rather than forwarding Bearer tokens.
        class RedirectHandler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(302)
                self.send_header("Location", "http://127.0.0.1:9/steal")
                self.end_headers()

            def log_message(self, *_args):
                pass

        srv = HTTPServer(("127.0.0.1", 0), RedirectHandler)
        t = threading.Thread(target=srv.serve_forever, daemon=True)
        t.start()
        try:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                node.http_json(f"http://127.0.0.1:{srv.server_port}/node/v1/work", "agn_tok")
            self.assertEqual(ctx.exception.code, 302)
        finally:
            srv.shutdown()
            srv.server_close()


if __name__ == "__main__":
    unittest.main()
