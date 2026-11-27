#!/usr/bin/env python3
"""Unified Agenct Execution Node (PC Node & Cloud Container Node).

Polls Agenct Hub using exclusive lease protocol 2, executes assigned jobs using
local coding agent CLIs (claude / codex / agy / opencode / custom --cmd),
renews leases with live progress, journals results for
crash-safe replay, and submits results + evidence attachments back to Hub.

Security defaults:
  - Sandbox defaults to 'workspace-write' (never full home-directory access).
  - Full host access requires explicitly passing --unsafe-full-access.
  - Zero hardcoded user/org directories.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import importlib
import json
import locale
import ntpath
import os
import platform
import re
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
from typing import Callable
import urllib.error
import urllib.request
import uuid

PROTOCOL_VERSION = "2"
CLI_AGENTS = ("claude", "codex", "agy", "opencode")
VALID_AGENTS = ("auto", *CLI_AGENTS)
VALID_EFFORTS = ("minimal", "low", "medium", "high", "xhigh", "max")
VALID_EXEC_BACKENDS = ("auto", "tmux", "conpty", "wt-tail", "pipe")
VALID_SANDBOX_BACKENDS = ("auto", "bwrap", "seatbelt", "cli-native")
EXEC_BACKEND_ENV = "AGENCT_EXEC_BACKEND"
SANDBOX_BACKEND_ENV = "AGENCT_SANDBOX_BACKEND"
_DEFAULT_MASKED_SECRET_NAMES = (
    ".ssh",
    ".agenct",
    ".aws",
    ".gnupg",
    ".kube",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".docker",
    os.path.join(".config", "gh"),
    os.path.join(".config", "glab-cli"),
    os.path.join(".config", "git", "credentials"),
)
MAX_RESULT_BYTES = 16_000
DEFAULT_NODE_TIMEOUT = 3600
NODE_TIMEOUT_ENV = "AGENCT_NODE_TIMEOUT"
DEFAULT_IDLE_TIMEOUT = 300
NODE_IDLE_TIMEOUT_ENV = "AGENCT_NODE_IDLE_TIMEOUT"
NODE_ALLOW_ELEVATED_ENV = "AGENCT_ALLOW_ELEVATED"
IDLE_WARN_SECS = 60
MAX_EVIDENCE_BYTES = 5 * 1024 * 1024
MAX_EVIDENCE_COUNT = 4
_SAFE_MODEL_RE = re.compile(r"^[A-Za-z0-9._:/@][A-Za-z0-9._:/@-]{0,127}$")
_SANDBOX_RANK = {"readonly": 0, "workspace-write": 1, "unsafe-full": 2}
_AGY_READONLY_DENY_RULES = [
    "write_file(*)",
    "unsandboxed(*)",
    "command(git commit)",
    "command(git push)",
    "command(git add)",
    "command(git checkout)",
    "command(git reset)",
    "command(git clean)",
    "command(rm)",
    "command(mv)",
    "command(sed -i)",
]


def _build_agy_readonly_settings(workdir: str, in_dir: str | None = None) -> str:
    allow = [f"read_file({workdir})"]
    if in_dir:
        allow.append(f"read_file({in_dir})")
    return json.dumps(
        {
            "enableTerminalSandbox": True,
            "toolPermission": "proceed-in-sandbox",
            "permissions": {
                "allow": allow,
                "deny": _AGY_READONLY_DENY_RULES,
            },
        },
        separators=(",", ":"),
    )


_AGY_WORKSPACE_SETTINGS = json.dumps(
    {
        "enableTerminalSandbox": True,
        "toolPermission": "proceed-in-sandbox",
    },
    separators=(",", ":"),
)
_OPENCODE_READONLY_PERMISSION = json.dumps(
    {"edit": "deny", "bash": "deny", "external_directory": "deny"},
    separators=(",", ":"),
)
_OPENCODE_WORKSPACE_PERMISSION = json.dumps({"external_directory": "deny"}, separators=(",", ":"))
_OPENCODE_DENIAL_LINE_RE = re.compile(
    r"^\s*!\s*permission requested:.*auto-rejecting",
    re.IGNORECASE | re.MULTILINE,
)

_ANSI_OSC_RE = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
_ANSI_CSI_RE = re.compile(r"\x1b\[[0-?]*[ -/]*[@-~]")
_ANSI_ESC_RE = re.compile(r"\x1b[@-Z\\-_]")
_CTRL_CHARS_RE = re.compile(r"[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]")


def strip_ansi(text: str) -> str:
    """Remove ANSI/VT100 terminal escape codes and stray non-printable control chars."""
    if not text:
        return ""
    cleaned = _ANSI_OSC_RE.sub("", text)
    cleaned = _ANSI_CSI_RE.sub("", cleaned)
    cleaned = _ANSI_ESC_RE.sub("", cleaned)
    cleaned = cleaned.replace("\r\n", "\n").replace("\r", "")
    return _CTRL_CHARS_RE.sub("", cleaned)


_SAFE_SESSION_RE = re.compile(r"^[A-Za-z0-9._-]{4,128}$")


def _valid_session_id(val: object) -> str | None:
    if isinstance(val, str):
        s = val.strip()
        if _SAFE_SESSION_RE.match(s):
            return s
    return None


def _iter_json_events(raw: str):
    for line in raw.splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(ev, dict) and ("type" in ev or "event" in ev):
            yield ev


def _extract_error_message(err_obj: object, fallback_msg: object = None) -> str:
    if isinstance(err_obj, dict):
        data = err_obj.get("data") if isinstance(err_obj.get("data"), dict) else {}
        msg = data.get("message") or err_obj.get("message") or err_obj.get("name")
        if msg:
            return str(msg)
    elif err_obj:
        return str(err_obj)
    if fallback_msg:
        return str(fallback_msg)
    return ""


def _format_denied_actions(denied: object, prefix: str = "Action denied in headless mode") -> str:
    if not isinstance(denied, list) or not denied:
        return ""
    labels = [
        str(d.get("display_name") or d.get("tool_name") or d.get("tool") or d.get("action") or d)
        if isinstance(d, dict)
        else str(d)
        for d in denied
    ]
    return f"{prefix}: {', '.join(labels)}"


def _combine_stream_body_and_errors(body: str | None, errors: list[str]) -> str:
    clean_body = strip_ansi(body).strip() if body else ""
    clean_err = strip_ansi("\n".join(errors)).strip()
    if clean_body and clean_err:
        return f"{clean_body}\n\n{clean_err}"
    return clean_body or clean_err


def _parse_opencode_stream(raw: str) -> tuple[str | None, str | None, bool]:
    """Extract (final_text, session_id, has_error) from `opencode run --format json`."""
    parsed_any = False
    msg_order: list[str] = []
    msg_texts: dict[str, list[str]] = {}
    errors: list[str] = []
    session_id: str | None = None

    for ev in _iter_json_events(raw):
        parsed_any = True
        part_obj = ev.get("part") if isinstance(ev.get("part"), dict) else {}
        session_id = _valid_session_id(ev.get("sessionID")) or _valid_session_id(part_obj.get("sessionID")) or session_id

        ev_type = ev.get("type")
        if ev_type == "text":
            txt = part_obj.get("text") if isinstance(part_obj.get("text"), str) else ev.get("text")
            if isinstance(txt, str) and txt.strip():
                mid = str(part_obj.get("messageID") or "")
                if mid not in msg_texts:
                    msg_order.append(mid)
                    msg_texts[mid] = []
                msg_texts[mid].append(txt)
        elif ev_type == "error":
            msg = _extract_error_message(ev.get("error"), ev.get("message"))
            if msg:
                errors.append(msg)

    if not parsed_any:
        return None, None, False
    body = "".join(msg_texts[msg_order[-1]]) if msg_order else None
    return _combine_stream_body_and_errors(body, errors), session_id, bool(errors)


def _parse_claude_stream(raw: str) -> tuple[str | None, str | None, bool]:
    """Extract (final_text, session_id, has_error) from `claude -p --output-format stream-json --verbose`."""
    parsed_any = False
    session_id: str | None = None
    result_text: str | None = None
    last_assistant_text: str | None = None
    errors: list[str] = []
    has_err = False

    for ev in _iter_json_events(raw):
        parsed_any = True
        session_id = _valid_session_id(ev.get("session_id")) or session_id
        ev_type = ev.get("type")
        if ev_type == "assistant":
            msg = ev.get("message") if isinstance(ev.get("message"), dict) else {}
            content = msg.get("content") if isinstance(msg.get("content"), list) else []
            texts = [
                blk["text"]
                for blk in content
                if isinstance(blk, dict) and blk.get("type") == "text" and isinstance(blk.get("text"), str)
            ]
            joined = "".join(texts).strip()
            if joined:
                last_assistant_text = joined
        elif ev_type == "result":
            res = ev.get("result")
            if isinstance(res, str) and res.strip():
                result_text = res
            is_error = bool(ev.get("is_error"))
            subtype = str(ev.get("subtype") or "").strip().lower()
            if is_error or subtype.startswith("error"):
                has_err = True
            errs = ev.get("errors")
            if isinstance(errs, list):
                for item in errs:
                    msg = _extract_error_message(item)
                    if msg:
                        errors.append(msg)
            elif ev.get("error"):
                msg = _extract_error_message(ev.get("error"))
                if msg:
                    errors.append(msg)
            denied_msg = _format_denied_actions(ev.get("permission_denials"))
            if denied_msg:
                errors.append(denied_msg)
        elif ev_type == "error":
            has_err = True
            msg = _extract_error_message(ev.get("error"), ev.get("message"))
            if msg:
                errors.append(msg)

    if not parsed_any:
        return None, None, False
    body = result_text if result_text is not None else last_assistant_text
    if errors:
        has_err = True
    return _combine_stream_body_and_errors(body, errors), session_id, has_err


def _parse_codex_stream(raw: str) -> tuple[str | None, str | None, bool]:
    """Extract (final_text, thread_id, has_error) from `codex exec --json` JSONL output."""
    parsed_any = False
    session_id: str | None = None
    last_agent_msg: str | None = None
    errors: list[str] = []
    has_err = False

    for ev in _iter_json_events(raw):
        parsed_any = True
        session_id = _valid_session_id(ev.get("thread_id")) or _valid_session_id(ev.get("session_id")) or session_id
        ev_type = ev.get("type")
        if ev_type == "item.completed":
            item = ev.get("item") if isinstance(ev.get("item"), dict) else {}
            if item.get("type") == "agent_message" and isinstance(item.get("text"), str) and item["text"].strip():
                last_agent_msg = item["text"]
        elif ev_type in ("error", "turn.failed"):
            has_err = True
            msg = _extract_error_message(ev.get("error"), ev.get("message"))
            if msg:
                errors.append(msg)

    if not parsed_any:
        return None, None, False
    return _combine_stream_body_and_errors(last_agent_msg, errors), session_id, has_err or bool(errors)


def _parse_agy_stream(raw: str) -> tuple[str | None, str | None, bool]:
    """Extract (final_text, conversation_id, has_error) from `agy -p ... --output-format stream-json` JSONL."""
    parsed_any = False
    session_id: str | None = None
    chunks: list[str] = []
    saw_tool = False
    result_fallback: str | None = None
    errors: list[str] = []

    for ev in _iter_json_events(raw):
        parsed_any = True
        ev_name = ev.get("event")
        su = ev.get("step_update") if isinstance(ev.get("step_update"), dict) else {}
        res = ev.get("result") if isinstance(ev.get("result"), dict) else {}
        session_id = (
            _valid_session_id(ev.get("conversation_id"))
            or _valid_session_id(su.get("conversation_id"))
            or _valid_session_id(res.get("conversation_id"))
            or session_id
        )
        if ev_name == "step_update":
            stype = su.get("step_type")
            if stype == "tool":
                saw_tool = True
            elif stype == "agent_response":
                txt = su.get("text_delta")
                if isinstance(txt, str) and txt:
                    if saw_tool:
                        chunks = [txt]
                        saw_tool = False
                    else:
                        chunks.append(txt)
        elif ev_name == "result":
            resp_txt = res.get("response")
            if isinstance(resp_txt, str) and resp_txt.strip():
                result_fallback = resp_txt
            res_status = str(res.get("status") or "").strip().upper()
            msg = _extract_error_message(res.get("error"))
            if msg:
                errors.append(msg)
            elif res_status and res_status != "SUCCESS":
                errors.append(f"agy finished with status {res_status}")
            denied_msg = _format_denied_actions(res.get("denied_actions"))
            if denied_msg:
                errors.append(denied_msg)
        elif ev_name == "error" or ev.get("error"):
            msg = _extract_error_message(ev.get("error"), ev.get("message"))
            if msg:
                errors.append(msg)

    if not parsed_any:
        return None, None, False
    body = result_fallback if result_fallback is not None else "".join(chunks)
    return _combine_stream_body_and_errors(body, errors), session_id, bool(errors)


def parse_cli_output(agent: str, raw: str) -> tuple[str | None, str | None]:
    adapter = CLI_ADAPTERS.get(agent)
    if adapter:
        text, sid, _ = adapter.parse_stream(raw)
        return text, sid
    return None, None


def detect_cli_stream_error(agent: str, raw: str, raw_err: str = "") -> bool:
    """Detect stream-reported errors/denials even when CLI exits with code 0."""
    adapter = CLI_ADAPTERS.get(agent)
    parsed_text: str | None = None
    if adapter:
        parsed_text, _, has_err = adapter.parse_stream(raw)
        if has_err:
            return True
    if _OPENCODE_DENIAL_LINE_RE.search(strip_ansi(raw_err)):
        return True
    if parsed_text is None and _OPENCODE_DENIAL_LINE_RE.search(strip_ansi(raw)):
        return True
    return False


_SANDBOX_BLOCK_PATTERNS = (
    re.compile(r"^\s*!\s*(permission requested:.*auto-rejecting.*)$", re.IGNORECASE | re.MULTILINE),
    re.compile(r"(Action denied(?:\s+in\s+headless\s+mode)?\s*:[^\r\n]+)", re.IGNORECASE),
    re.compile(r"(Sandbox blocked[^\r\n]+)", re.IGNORECASE),
    re.compile(r"(Read-only file system[^\r\n]*|Operation not permitted[^\r\n]*|Permission denied[^\r\n]*|Host key verification failed[^\r\n]*|Could not read from remote repository[^\r\n]*)", re.IGNORECASE),
    re.compile(r"([^\r\n]*(?:network_access|outside the workspace|sandbox restriction)[^\r\n]*)", re.IGNORECASE),
)

def _clean_blocked_reason(msg: str) -> str:
    cleaned = " ".join(msg.replace("`", "'").split()).lstrip("! ").rstrip("。.）) ").strip()
    return cleaned[:160]


def detect_sandbox_block(agent: str, raw_out: str, raw_err: str = "", text: str = "") -> str:
    """Return a concise 1-line sandbox/permission block reason when a job failed due to isolation."""
    haystacks = (
        strip_ansi(raw_err),
        text,
        strip_ansi(raw_out),
    )
    for hay in haystacks:
        if not hay:
            continue
        for pat in _SANDBOX_BLOCK_PATTERNS:
            m = pat.search(hay)
            if m:
                msg = _clean_blocked_reason(m.group(1))
                if msg:
                    return msg
        if agent == "opencode" and "rule which prevents you from using this specific tool call" in hay:
            return "OpenCode tool permission blocked directory or command access"
    return ""


def _summarize_opencode_event(ev: dict) -> str:
    ev_type = ev.get("type")
    if ev_type == "tool_use":
        part = ev.get("part") if isinstance(ev.get("part"), dict) else {}
        tool = str(part.get("tool") or "tool")
        state = part.get("state") if isinstance(part.get("state"), dict) else {}
        inp = state.get("input") if isinstance(state.get("input"), dict) else {}
        detail = state.get("title") or inp.get("command") or inp.get("path") or ""
        summary = f"{tool}: {detail}" if detail else tool
        return strip_ansi(str(summary)).strip()
    if ev_type == "text":
        part = ev.get("part") if isinstance(ev.get("part"), dict) else {}
        txt = part.get("text") or ev.get("text") or ""
        return strip_ansi(str(txt)).strip()
    if ev_type == "error":
        return strip_ansi(_extract_error_message(ev.get("error"), ev.get("message")) or "error").strip()
    return ""


def _summarize_claude_event(ev: dict) -> str:
    ev_type = ev.get("type")
    if ev_type == "assistant":
        msg = ev.get("message") if isinstance(ev.get("message"), dict) else {}
        content = msg.get("content") if isinstance(msg.get("content"), list) else []
        for blk in content:
            if not isinstance(blk, dict):
                continue
            if blk.get("type") == "tool_use":
                name = str(blk.get("name") or "tool")
                inp = blk.get("input") if isinstance(blk.get("input"), dict) else {}
                detail = (
                    inp.get("command")
                    or inp.get("file_path")
                    or inp.get("path")
                    or inp.get("pattern")
                    or inp.get("query")
                    or ""
                )
                return strip_ansi(f"{name}: {detail}" if detail else name).strip()
        for blk in content:
            if isinstance(blk, dict) and blk.get("type") == "text" and blk.get("text"):
                return strip_ansi(str(blk["text"])).strip()
        return ""
    if ev_type == "result":
        if ev.get("is_error"):
            return strip_ansi(str(ev.get("result") or ev.get("error") or "error")).strip()
        denied = _format_denied_actions(ev.get("permission_denials"), prefix="Action denied")
        if denied:
            return strip_ansi(denied).strip()
        if isinstance(ev.get("result"), str):
            return strip_ansi(ev["result"]).strip()
        return ""
    if ev_type == "error":
        return strip_ansi(_extract_error_message(ev.get("error"), ev.get("message")) or "error").strip()
    return ""


def _summarize_codex_event(ev: dict) -> str:
    ev_type = ev.get("type")
    if ev_type in ("item.started", "item.completed"):
        item = ev.get("item") if isinstance(ev.get("item"), dict) else {}
        itype = item.get("type")
        if itype == "command_execution":
            cmd = item.get("command") or ""
            return strip_ansi(f"bash: {cmd}" if cmd else "bash").strip()
        if itype == "file_change":
            changes = item.get("changes") if isinstance(item.get("changes"), list) else []
            first_path = ""
            if changes and isinstance(changes[0], dict):
                first_path = str(changes[0].get("path") or "")
            return strip_ansi(f"edit: {first_path}" if first_path else "file_change").strip()
        if itype == "mcp_tool_call":
            tool = item.get("tool") or item.get("server") or "mcp"
            return strip_ansi(str(tool)).strip()
        if itype == "agent_message" and isinstance(item.get("text"), str):
            return strip_ansi(item["text"]).strip()
        return ""
    if ev_type in ("error", "turn.failed"):
        return strip_ansi(_extract_error_message(ev.get("error"), ev.get("message")) or "error").strip()
    return ""


def _summarize_agy_event(ev: dict) -> str:
    ev_name = ev.get("event")
    if ev_name == "step_update":
        su = ev.get("step_update") if isinstance(ev.get("step_update"), dict) else {}
        stype = su.get("step_type")
        if stype == "tool":
            state = str(su.get("state") or "").upper()
            if state == "DONE":
                return ""
            info = su.get("tool_info") if isinstance(su.get("tool_info"), dict) else {}
            name = str(su.get("tool_name") or info.get("name") or "tool")
            parms = info.get("parameters") if isinstance(info.get("parameters"), dict) else {}
            summary = parms.get("toolSummary")
            if isinstance(summary, str) and summary.strip():
                return strip_ansi(summary).strip()
            detail = (
                parms.get("CommandLine")
                or parms.get("Path")
                or parms.get("Query")
                or parms.get("command")
                or parms.get("file_path")
                or parms.get("path")
                or parms.get("query")
                or ""
            )
            return strip_ansi(f"{name}: {detail}" if detail else name).strip()
        return ""
    if ev_name == "result":
        res = ev.get("result") if isinstance(ev.get("result"), dict) else {}
        if res.get("error"):
            return strip_ansi(str(res["error"])).strip()
        denied = _format_denied_actions(res.get("denied_actions"), prefix="Action denied")
        if denied:
            return strip_ansi(denied).strip()
        if isinstance(res.get("response"), str) and res["response"].strip():
            return strip_ansi(res["response"]).strip()
        return ""
    if ev_name == "error" or ev.get("error"):
        return strip_ansi(_extract_error_message(ev.get("error"), ev.get("message")) or "error").strip()
    return ""


_PROGRESS_TOOL_LABELS = {
    "read_file": "查看",
    "view_file": "查看",
    "read": "查看",
    "edit_file": "修改",
    "write_file": "修改",
    "write_to_file": "修改",
    "replace_file_content": "修改",
    "multi_replace_file_content": "修改",
    "edit": "修改",
    "write": "修改",
    "grep_search": "检索",
    "grep": "检索",
    "find_by_name": "检索",
    "glob": "检索",
    "list_dir": "检索",
    "run_command": "执行",
    "bash": "执行",
}


def format_progress_entry(text: str, workdir: str = "", max_chars: int = 84) -> str:
    """Format a raw progress line into a concise, human-readable step without mid-line left clipping."""
    collapsed = " ".join(strip_ansi(text).split())
    if not collapsed:
        return ""

    prefixes: list[str] = []
    if workdir:
        for base in dict.fromkeys((os.path.abspath(workdir), os.path.realpath(workdir))):
            clean_base = base.rstrip("/")
            if clean_base and clean_base != "/":
                prefixes.append(clean_base + "/")

    def _shorten(val: str) -> str:
        out_val = val
        for pfx in prefixes:
            out_val = out_val.replace(pfx, "")
        return out_val

    if ": " in collapsed:
        tool_raw, detail = collapsed.split(": ", 1)
    else:
        tool_raw, detail = collapsed, ""
    tool_key = tool_raw.strip().lower()
    if tool_key == "update_steps":
        return ""
    if tool_key == "exit_plan_mode":
        return "规划完成，开始执行"

    cn_action = _PROGRESS_TOOL_LABELS.get(tool_key)
    if cn_action:
        body = _shorten(detail.strip())
        if cn_action == "执行" and body:
            body = re.sub(r"^cd\s+\S+\s*(?:&&|;)\s*", "", body).strip()
        out = f"{cn_action} {body}".strip() if body else cn_action
    else:
        out = _shorten(collapsed)

    if max_chars >= 4 and len(out) > max_chars:
        out = out[: max_chars - 1].rstrip() + "…"
    return out


def summarize_progress_line(agent: str, raw_line: str) -> str:
    """Turn a raw stdout/stderr line into clean human-readable progress text."""
    stripped = raw_line.strip()
    adapter = CLI_ADAPTERS.get(agent)
    if adapter and stripped.startswith("{"):
        try:
            ev = json.loads(stripped)
        except json.JSONDecodeError:
            ev = None
        if isinstance(ev, dict) and ("type" in ev or "event" in ev):
            return adapter.summarize_event(ev)
    return strip_ansi(raw_line).strip()

EVIDENCE_MIME = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".webp": "image/webp",
    ".gif": "image/gif",
    ".pdf": "application/pdf",
    ".txt": "text/plain",
    ".log": "text/plain",
    ".md": "text/markdown",
    ".csv": "text/csv",
    ".json": "application/json",
}


def configure_stdio() -> None:
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, "reconfigure"):
            try:
                stream.reconfigure(encoding="utf-8", errors="replace")
            except Exception:
                pass


def decode_bytes(raw: bytes | None) -> str:
    if not raw:
        return ""
    try:
        return raw.decode("utf-8")
    except UnicodeDecodeError:
        enc = locale.getpreferredencoding(False) or "utf-8"
        return raw.decode(enc, errors="replace")


def truncate_utf8(text: str, max_bytes: int = MAX_RESULT_BYTES) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    suffix = "\n\n...(output truncated)"
    budget = max(0, max_bytes - len(suffix.encode("utf-8")))
    return encoded[:budget].decode("utf-8", errors="ignore") + suffix


class _NoRedirectHandler(urllib.request.HTTPRedirectHandler):
    """Refuse HTTP 3xx redirects so Bearer tokens never leak or downgrade."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


_HTTP_OPENER = urllib.request.build_opener(_NoRedirectHandler())

_SCRUBBED_CHILD_ENV_KEYS = (
    "AGENCT_NODE_TOKEN",
    "AGENCT_CLOUD_NODE_TOKEN",
    "AGENCT_MASTER_KEY",
    "AGENCT_MASTER_KEY_FILE",
    "AGENCT_MASTER_KEY_OLD",
    "AGENCT_MASTER_KEY_OLD_FILE",
    "AGENCT_LOGIN_TOKEN",
    "AGENCT_GITLAB_CLIENT_SECRET",
    "AGENCT_GITLAB_CLIENT_SECRET_FILE",
    "AGENCT_GITLAB_WEBHOOK_SECRET",
)

_SANDBOX_SCRUBBED_ENV_KEYS = (
    "SSH_AUTH_SOCK",
    "SSH_AGENT_PID",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GH_ENTERPRISE_TOKEN",
    "GITHUB_ENTERPRISE_TOKEN",
    "GITLAB_TOKEN",
    "GITLAB_PRIVATE_TOKEN",
    "GLAB_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "NPM_TOKEN",
)


def _build_child_base_env(
    base_env: dict[str, str] | None = None,
    *,
    sandbox: str = "workspace-write",
) -> dict[str, str]:
    env = dict(os.environ if base_env is None else base_env)
    for key in _SCRUBBED_CHILD_ENV_KEYS:
        env.pop(key, None)
    if sandbox != "unsafe-full":
        for key in _SANDBOX_SCRUBBED_ENV_KEYS:
            env.pop(key, None)
    return env


def http_json(
    url: str,
    token: str,
    method: str = "GET",
    body: dict | None = None,
    extra_headers: dict[str, str] | None = None,
    timeout: int = 20,
) -> dict:
    data = json.dumps(body, ensure_ascii=False).encode("utf-8") if body is not None else None
    headers = {
        "Accept": "application/json",
        "Authorization": f"Bearer {token}",
        "X-Agenct-Node-Protocol": PROTOCOL_VERSION,
    }
    if data is not None:
        headers["Content-Type"] = "application/json"
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with _HTTP_OPENER.open(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def discover_available_agents() -> list[str]:
    """Return all locally available CLI engines in priority order."""
    return [tool for tool in CLI_AGENTS if shutil.which(tool)]


def resolve_node_agents(
    preferred: str = "auto",
    allowed_spec: str = "",
    cmd: str | None = None,
) -> tuple[str, list[str]]:
    """Resolve (default_agent, allowed_agents) for this host.

    - Custom --cmd runs exclusively as 'custom:<cmd>'.
    - Without --agents, an explicit --agent stays pinned to that single engine (safe default),
      while '--agent auto' uses '--agents' if provided or all discovered CLI engines.
    - With '--agents auto' or '--agents claude,codex,...', all listed/discovered engines are enabled.
    """
    if cmd:
        custom = f"custom:{cmd}"
        return custom, [custom]

    pref = (preferred or "auto").strip().lower()
    spec = (allowed_spec or "").strip().lower()
    explicit_pref = pref != "auto" and pref in VALID_AGENTS

    if spec in ("auto", "all"):
        allowed = discover_available_agents()
    elif spec:
        allowed = []
        for item in spec.split(","):
            name = item.strip().lower()
            if name and name != "auto" and name in VALID_AGENTS and name not in allowed:
                allowed.append(name)
        if not allowed:
            allowed = discover_available_agents()
    elif explicit_pref:
        allowed = [pref]
    else:
        allowed = discover_available_agents()

    if explicit_pref:
        if pref not in allowed:
            allowed = [pref, *allowed]
        default_agent = pref
    elif allowed:
        default_agent = allowed[0]
    else:
        default_agent = ""
    return default_agent, allowed


def detect_agent(preferred: str = "auto", cmd: str | None = None) -> str:
    agent, _ = resolve_node_agents(preferred, "", cmd)
    return agent


def select_job_agent(
    requested: object,
    default_agent: str,
    allowed_agents: list[str],
) -> tuple[str | None, str]:
    """Validate Hub-requested engine against this node's local allowlist."""
    raw = str(requested or "").strip().lower()
    if not raw:
        return default_agent, ""
    if raw in allowed_agents:
        return raw, ""
    if raw == "custom" and any(a.startswith("custom:") for a in allowed_agents):
        return next(a for a in allowed_agents if a.startswith("custom:")), ""
    allowed_labels = ", ".join("custom" if a.startswith("custom:") else a for a in allowed_agents)
    return None, f"Requested engine '{raw}' is not enabled on this node (allowed: {allowed_labels})."


def build_env_header(
    kind: str,
    agent: str,
    workdir: str,
    agents: list[str] | None = None,
    *,
    sandbox: str = "workspace-write",
    isolation: str = "",
    allow_elevated: bool = False,
    codex_tmux: bool = False,
) -> str:
    host = re.sub(r"[^A-Za-z0-9._-]", "_", socket.gethostname())[:48]
    os_name = platform.system().lower()
    repos = []
    if os.path.isdir(workdir):
        try:
            for name in sorted(os.listdir(workdir)):
                if name.startswith(".") or not re.fullmatch(r"[A-Za-z0-9._-]+", name):
                    continue
                if os.path.isdir(os.path.join(workdir, name, ".git")):
                    repos.append(name)
                if len(repos) >= 20:
                    break
        except OSError:
            pass
    default_label = "custom" if agent.startswith("custom:") else agent
    agent_labels: list[str] = []
    for item in (agents or [agent]):
        lbl = "custom" if item.startswith("custom:") else item
        if lbl and lbl not in agent_labels:
            agent_labels.append(lbl)
    if default_label and default_label not in agent_labels:
        agent_labels.insert(0, default_label)
    sb_clean = sandbox if sandbox in _SANDBOX_RANK else "workspace-write"
    iso_clean = "direct" if sb_clean == "unsafe-full" else isolation.strip().lower()
    extra_flags = []
    if iso_clean:
        extra_flags.append(f"isolation:{iso_clean}")
    if allow_elevated:
        extra_flags.append("elevated:1")
    if codex_tmux and sb_clean == "workspace-write" and iso_clean not in ("bwrap", "seatbelt"):
        extra_flags.append("codex_net:1")
    flag_part = f" | {' | '.join(extra_flags)}" if extra_flags else ""
    repo_part = f" | repos:{','.join(repos)}" if repos else ""
    summary = (
        f"kind:{kind} | os:{os_name} | host:{host} | agent:{default_label}"
        f" | agents:{','.join(agent_labels)} | sandbox:{sb_clean}{flag_part}{repo_part}"
    )
    return summary[:560]


class ExecutionJournal:
    """Crash-safe per-task journal: replays completed submissions, never auto-reruns uncertain jobs."""

    def __init__(self, state_dir: str) -> None:
        self.dir = os.path.join(state_dir, "journal")
        os.makedirs(self.dir, mode=0o700, exist_ok=True)

    def _path(self, task_id: str, rev: int) -> str:
        safe = hashlib.sha256(f"{task_id}:{rev}".encode("utf-8")).hexdigest()[:32]
        return os.path.join(self.dir, f"{safe}.json")

    def write(self, task_id: str, rev: int, record: dict) -> None:
        path = self._path(task_id, rev)
        tmp = f"{path}.tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(record, fh, ensure_ascii=False)
        try:
            os.chmod(tmp, 0o600)
        except OSError:
            pass
        os.replace(tmp, path)

    def remove(self, task_id: str, rev: int) -> None:
        try:
            os.unlink(self._path(task_id, rev))
        except FileNotFoundError:
            pass

    def pending_submissions(self) -> list[dict]:
        out: list[dict] = []
        for name in sorted(os.listdir(self.dir)):
            if not name.endswith(".json"):
                continue
            path = os.path.join(self.dir, name)
            try:
                with open(path, "r", encoding="utf-8") as fh:
                    rec = json.load(fh)
                if rec.get("state") == "ready_to_submit":
                    out.append(rec)
            except Exception:
                continue
        return out


def make_scoped_tempdir(prefix: str, workdir: str | None = None, sandbox: str = "workspace-write") -> str:
    """Create a private temp dir inside workdir when writable to avoid CLI external_directory rejections."""
    if workdir and sandbox != "readonly" and os.path.isdir(workdir):
        try:
            path = tempfile.mkdtemp(prefix=f".{prefix}", dir=workdir)
            os.chmod(path, 0o700)
            return path
        except OSError:
            pass
    path = tempfile.mkdtemp(prefix=prefix)
    os.chmod(path, 0o700)
    return path


def materialize_attachments(
    attachments: list[dict],
    workdir: str | None = None,
    sandbox: str = "workspace-write",
) -> tuple[list[str], str | None]:
    if not attachments:
        return [], None
    temp_dir = make_scoped_tempdir("agenct-in-", workdir, sandbox)
    paths: list[str] = []
    for idx, att in enumerate(attachments[:4]):
        b64 = att.get("data_base64") or ""
        try:
            raw = base64.b64decode(b64, validate=True)
        except Exception:
            continue
        if not raw or len(raw) > MAX_EVIDENCE_BYTES:
            continue
        raw_name = str(att.get("name") or f"file_{idx}")
        safe = re.sub(r"[^A-Za-z0-9._-]", "_", raw_name)[:48].strip("._") or f"file_{idx}"
        target = os.path.join(temp_dir, f"{idx}_{safe}")
        with open(target, "wb") as fh:
            fh.write(raw)
        try:
            os.chmod(target, 0o600)
        except OSError:
            pass
        paths.append(target)
    if not paths:
        shutil.rmtree(temp_dir, ignore_errors=True)
        return [], None
    return paths, temp_dir


def collect_evidence(evidence_dir: str) -> list[dict]:
    if not os.path.isdir(evidence_dir) or os.path.islink(evidence_dir):
        return []
    base_real = os.path.realpath(evidence_dir)
    no_follow = getattr(os, "O_NOFOLLOW", None)
    out: list[dict] = []
    for name in sorted(os.listdir(evidence_dir)):
        ext = os.path.splitext(name)[1].lower()
        mime = EVIDENCE_MIME.get(ext)
        if not mime:
            continue
        full = os.path.join(evidence_dir, name)
        if os.path.islink(full):
            continue
        try:
            real = os.path.realpath(full)
            if os.path.commonpath([base_real, real]) != base_real or real == base_real:
                continue
            if no_follow is not None:
                fd = os.open(full, os.O_RDONLY | no_follow)
                try:
                    st = os.fstat(fd)
                    if not stat.S_ISREG(st.st_mode) or st.st_size <= 0 or st.st_size > MAX_EVIDENCE_BYTES:
                        continue
                    with os.fdopen(fd, "rb") as fh:
                        fd = -1
                        raw = fh.read(MAX_EVIDENCE_BYTES + 1)
                finally:
                    if fd >= 0:
                        os.close(fd)
            else:
                if not os.path.isfile(full) or os.path.getsize(full) > MAX_EVIDENCE_BYTES:
                    continue
                with open(full, "rb") as fh:
                    raw = fh.read(MAX_EVIDENCE_BYTES + 1)
            if not raw or len(raw) > MAX_EVIDENCE_BYTES:
                continue
            out.append({
                "name": name,
                "mime_type": mime,
                "data_base64": base64.b64encode(raw).decode("ascii"),
            })
        except OSError:
            continue
        if len(out) >= MAX_EVIDENCE_COUNT:
            break
    return out


def build_prompt(
    job: dict,
    input_files: list[str],
    evidence_dir: str,
    workdir: str,
    *,
    sandbox: str = "",
    agent: str = "",
    os_isolated: bool = False,
) -> str:
    parts = [
        "You are an Agenct execution node handling an assigned task.",
        "Complete the request concisely using available tools and write naturally in concise Markdown (≤10 lines unless detailed output is requested). By default, act within existing authorization and give the conclusion without asking a clarifying question. Only when there are at least two active threads or node receipts that can be followed up on and you cannot uniquely determine which one the user means, ask one clarifying question as the first sentence (e.g. 'Do you mean A or B?') and do not call `dispatch_node` or any other state-changing tool in that turn. Follow the default rule for routine dispatch or when missing details can be reasonably assumed. State the key facts concisely, and include concrete file:line, commits, or links briefly at the end when applicable. Do NOT wrap paragraphs in rigid form labels like 'Approval:'/'Conclusion:'/'Sources:', and avoid bloated essays or large tables unless explicitly requested.",
        "Stay strictly scoped to the instruction; do not inspect unrelated directories or sensitive files (such as ~/.ssh, credentials, or secrets) unless explicitly requested.",
        f"Working directory: {workdir}",
        f"Title: {job.get('title') or ''}",
        f"Instruction:\n{job.get('instruction') or ''}",
    ]
    if job.get("context"):
        parts.append(f"Context:\n{job['context']}")
    if input_files:
        parts.append("Input attachments saved locally:\n" + "\n".join(f"- {p}" for p in input_files))
    adapter = CLI_ADAPTERS.get(agent)
    if sandbox == "readonly":
        if os_isolated:
            parts.append(
                "Read-only OS sandbox is active: the filesystem outside /tmp is mounted read-only; "
                "read-only inspection commands (such as git status/log/diff/show, ls, rg, cat) work normally, "
                "while any file write or modification is blocked by the OS."
            )
        elif adapter and adapter.readonly_disables_shell:
            parts.append(
                "Read-only sandbox is active: file writes and shell commands are disabled; "
                "use built-in read, list, and search tools directly."
            )
        elif agent == "agy":
            parts.append(
                f"Read-only sandbox is active: {workdir} is mounted read-only "
                f"(use absolute paths or 'cd {workdir}' before read-only shell commands like git log/status)."
            )
    if sandbox != "unsafe-full":
        parts.append(
            "Host credentials (~/.ssh, ~/.config/gh, SSH_AUTH_SOCK, git/cloud tokens) are intentionally isolated in this sandbox; "
            "if an action requires host credentials (such as git push or remote SSH), report that clearly instead of probing alternative tokens or credential helpers."
        )
    parts.append(
        f"For text queries, output the final Markdown answer directly without creating files. "
        f"Only save screenshots or downloadable artifact files to {evidence_dir} if explicitly requested."
    )
    return "\n\n".join(parts)


class IdleTimeoutExpired(subprocess.TimeoutExpired):
    """Raised when a child process produces no stdout/stderr output for `idle_timeout` seconds."""


def _kill_process_group(pid: int, sig: int) -> None:
    if hasattr(os, "killpg"):
        try:
            os.killpg(pid, sig)
        except ProcessLookupError:
            pass
        except PermissionError:
            pass
    else:
        try:
            os.kill(pid, sig)
        except OSError:
            pass


def is_windows(system: str | None = None) -> bool:
    """Return True only on native Windows (never on Linux, macOS, or WSL2)."""
    sys_name = (platform.system() if system is None else system).strip().lower()
    return sys_name.startswith("win")


def is_wsl2(
    system: str | None = None,
    release: str | None = None,
    env: dict[str, str] | None = None,
) -> bool:
    """Return True when running inside WSL2 on Linux."""
    if is_windows(system):
        return False
    sys_name = (platform.system() if system is None else system).strip().lower()
    if sys_name != "linux":
        return False
    rel = (platform.release() if release is None else release).lower()
    eff_env = os.environ if env is None else env
    return "microsoft" in rel or bool(eff_env.get("WSL_DISTRO_NAME")) or bool(eff_env.get("WSL_INTEROP"))


def get_runs_dir(system: str | None = None, env: dict[str, str] | None = None) -> str:
    """Return execution log directory: %USERPROFILE%\\.agenct\\runs on Windows, /tmp/agenct-runs on POSIX."""
    eff_env = os.environ if env is None else env
    if is_windows(system):
        profile = (eff_env.get("USERPROFILE") or "").strip() or os.path.expanduser("~")
        if "\\" in profile or (len(profile) >= 2 and profile[1] == ":"):
            return ntpath.join(profile, ".agenct", "runs")
        return os.path.join(profile, ".agenct", "runs")
    return os.path.join(tempfile.gettempdir(), "agenct-runs")


def _sanitize_task_label(task_id: str) -> str:
    cleaned = re.sub(r"[^A-Za-z0-9._-]", "_", str(task_id or "").strip()).strip("._-")
    return (cleaned or "task")[:40]


def build_wt_tail_argv(wt_bin: str, task_id: str, log_path: str) -> list[str]:
    """Build strictly whitelisted Windows Terminal tail command argv."""
    if not wt_bin or "\n" in log_path or "\r" in log_path or "\x00" in log_path:
        raise ValueError("Invalid wt-tail arguments")
    safe_task = _sanitize_task_label(task_id)
    if any(ch in log_path for ch in (" ", "\t", "'", '"', ";", "|", "&", "$", "`", "(", ")")):
        escaped = log_path.replace("'", "''")
        ps_cmd = f"Get-Content -Wait '{escaped}'"
    else:
        ps_cmd = f"Get-Content -Wait {log_path}"
    return [
        wt_bin,
        "-w",
        "0",
        "new-tab",
        "--title",
        f"agenct:{safe_task}",
        "pwsh",
        "-NoExit",
        "-Command",
        ps_cmd,
    ]


def probe_tmux(
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
    socket_path: str | None = None,
) -> bool:
    """Probe whether tmux is installed and its server socket is usable on POSIX/WSL2."""
    if is_windows(system):
        return False
    which_fn = which or shutil.which
    tmux_bin = which_fn("tmux")
    if not tmux_bin:
        return False
    run_fn = runner or subprocess.run
    try:
        ver = run_fn([tmux_bin, "-V"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=3)
        if ver.returncode != 0:
            return False
        ls_cmd = [tmux_bin, "-S", socket_path, "list-sessions"] if socket_path else [tmux_bin, "list-sessions"]
        ls_res = run_fn(ls_cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=3)
        if ls_res.returncode == 0:
            return True
        combined = (decode_bytes(ls_res.stdout) + "\n" + decode_bytes(ls_res.stderr)).lower()
        for err_marker in (
            "unsafe permissions",
            "operation not permitted",
            "permission denied",
            "can't create socket",
            "cannot create socket",
        ):
            if err_marker in combined:
                return False
        return True
    except Exception:
        return False


def probe_conpty(
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    import_module: Callable[[str], object] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
) -> str | None:
    """Probe Windows ConPTY provider ('pywinpty' or 'node-pty'); returns None on non-Windows or failure."""
    if not is_windows(system):
        return None
    imp = import_module or importlib.import_module
    try:
        mod = imp("winpty")
        if hasattr(mod, "PtyProcess") or hasattr(mod, "PTY"):
            return "pywinpty"
    except Exception:
        pass
    which_fn = which or shutil.which
    node_bin = which_fn("node")
    if node_bin:
        run_fn = runner or subprocess.run
        try:
            res = run_fn(
                [node_bin, "-e", "require.resolve('node-pty')"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=3,
            )
            if res.returncode == 0:
                return "node-pty"
        except Exception:
            pass
    return None


def probe_wt_exe(
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
) -> str | None:
    """Probe native Windows wt.exe availability (rejects non-Windows/WSL2 /mnt/ paths and broken Store aliases)."""
    if not is_windows(system):
        return None
    which_fn = which or shutil.which
    wt_bin = which_fn("wt.exe") or which_fn("wt")
    if not wt_bin:
        return None
    if wt_bin.replace("\\", "/").lower().startswith("/mnt/"):
        return None
    run_fn = runner or subprocess.run
    try:
        res = run_fn([wt_bin, "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=3)
        if res.returncode != 0:
            return None
        return wt_bin
    except Exception:
        return None


_WRAPPER_SCRIPT = r"""
import json, os, subprocess, sys, threading

run_dir = sys.argv[1]
spec_path = os.path.join(run_dir, "spec.json")
with open(spec_path, "r", encoding="utf-8") as fh:
    spec = json.load(fh)
try:
    os.unlink(spec_path)
except OSError:
    pass

stdout_path = os.path.join(run_dir, "stdout.log")
stderr_path = os.path.join(run_dir, "stderr.log")
pid_path = os.path.join(run_dir, "child.pid")
exit_path = os.path.join(run_dir, "exit.code")

popen_kwargs = dict(
    shell=bool(spec.get("shell")),
    cwd=spec["cwd"],
    env=spec["env"],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.PIPE,
    stderr=subprocess.PIPE,
)
if os.name != "nt":
    popen_kwargs["start_new_session"] = True
else:
    popen_kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)

proc = subprocess.Popen(spec["argv"], **popen_kwargs)
pid_tmp = pid_path + ".tmp"
with open(pid_tmp, "w", encoding="utf-8") as fh:
    fh.write(str(proc.pid))
os.replace(pid_tmp, pid_path)

def pump(stream, log_path, mirror):
    if stream is None:
        return
    fd = stream.fileno()
    with open(log_path, "ab", buffering=0) as log_fh:
        while True:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            log_fh.write(chunk)
            if mirror is not None:
                try:
                    mirror.write(chunk)
                    mirror.flush()
                except Exception:
                    pass

t_out = threading.Thread(
    target=pump, args=(proc.stdout, stdout_path, getattr(sys.stdout, "buffer", None)), daemon=True
)
t_err = threading.Thread(
    target=pump, args=(proc.stderr, stderr_path, getattr(sys.stderr, "buffer", None)), daemon=True
)
t_out.start()
t_err.start()
rc = proc.wait()
t_out.join(timeout=2)
t_err.join(timeout=2)

exit_tmp = exit_path + ".tmp"
with open(exit_tmp, "w", encoding="utf-8") as fh:
    fh.write(str(rc))
os.replace(exit_tmp, exit_path)
sys.exit(rc)
"""

_NODE_PTY_HOST_SCRIPT = r"""
const pty = require("node-pty");
const pyExe = process.argv[2];
const wrapperPath = process.argv[3];
const runDir = process.argv[4];
const cwd = process.argv[5] || process.cwd();
const term = pty.spawn(pyExe, [wrapperPath, runDir], {
  name: "xterm-color",
  cols: 120,
  rows: 30,
  cwd,
  env: process.env,
});
term.onData(() => {});
term.onExit(({ exitCode }) => {
  process.exit(typeof exitCode === "number" ? exitCode : 0);
});
"""


def _prepare_wrapper_run_dir(
    runs_dir: str,
    prefix: str,
    argv: list[str] | str,
    *,
    shell: bool,
    cwd: str,
    env: dict[str, str],
) -> tuple[str, str, str, str]:
    os.makedirs(runs_dir, exist_ok=True)
    run_dir = tempfile.mkdtemp(prefix=f"{prefix}-", dir=runs_dir)
    try:
        os.chmod(run_dir, 0o700)
    except OSError:
        pass
    spec_path = os.path.join(run_dir, "spec.json")
    wrapper_path = os.path.join(run_dir, "wrapper.py")
    stdout_path = os.path.join(run_dir, "stdout.log")
    stderr_path = os.path.join(run_dir, "stderr.log")
    with open(spec_path, "w", encoding="utf-8") as fh:
        json.dump({"argv": argv, "shell": shell, "cwd": cwd, "env": env}, fh)
    with open(wrapper_path, "w", encoding="utf-8") as fh:
        fh.write(_WRAPPER_SCRIPT)
    for path in (spec_path, wrapper_path, stdout_path, stderr_path):
        if not os.path.exists(path):
            open(path, "wb").close()
        try:
            os.chmod(path, 0o600)
        except OSError:
            pass
    return run_dir, wrapper_path, stdout_path, stderr_path


def _read_int_file(path: str) -> int | None:
    try:
        with open(path, "r", encoding="utf-8") as fh:
            raw = fh.read().strip()
        return int(raw) if raw else None
    except (OSError, ValueError):
        return None


def _terminate_proc_or_pg(proc: subprocess.Popen | None, pid: int | None, *, force: bool) -> None:
    if not is_windows() and hasattr(os, "killpg"):
        target_pid = pid if pid is not None else (proc.pid if proc is not None else None)
        if target_pid is None:
            return
        if force:
            _kill_process_group(target_pid, signal.SIGKILL)
            if proc is not None:
                try:
                    proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    pass
            return
        _kill_process_group(target_pid, signal.SIGTERM)
        if proc is not None:
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                _kill_process_group(target_pid, signal.SIGKILL)
                proc.wait()
        else:
            deadline = time.monotonic() + 3.0
            while time.monotonic() < deadline:
                try:
                    os.kill(target_pid, 0)
                except OSError:
                    return
                time.sleep(0.05)
            _kill_process_group(target_pid, signal.SIGKILL)
        return

    if proc is not None:
        if force:
            try:
                proc.kill()
            except OSError:
                pass
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                pass
        else:
            try:
                proc.terminate()
            except OSError:
                pass
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                try:
                    proc.kill()
                except OSError:
                    pass
                proc.wait()
    elif pid is not None:
        try:
            os.kill(pid, signal.SIGTERM)
        except OSError:
            pass


class ExecHandle:
    """Opaque handle returned by ExecBackend.spawn()."""

    def __init__(
        self,
        *,
        backend: str,
        session_id: str,
        argv: list[str] | str,
        log_path: str = "",
        stdout_path: str = "",
        stderr_path: str = "",
        pid: int | None = None,
        proc: subprocess.Popen | None = None,
        run_dir: str = "",
        meta: dict | None = None,
    ) -> None:
        self.backend = backend
        self.session_id = session_id
        self.argv = argv
        self.log_path = log_path
        self.stdout_path = stdout_path
        self.stderr_path = stderr_path
        self.pid = pid
        self.proc = proc
        self.run_dir = run_dir
        self.meta: dict = dict(meta or {})
        self.returncode: int | None = None


class BackendLogStream:
    """Minimal streaming log interface returned by ExecBackend.log(handle)."""

    path: str = ""

    def read_chunks(self) -> tuple[bytes, bytes]:
        raise NotImplementedError

    def is_done(self) -> bool:
        raise NotImplementedError

    def returncode(self) -> int | None:
        raise NotImplementedError

    def wait_event(self, timeout: float = 0.05) -> None:
        time.sleep(timeout)

    def wait_drain(self, timeout: float = 2.0) -> tuple[bytes, bytes]:
        return self.read_chunks()


class PipeLogStream(BackendLogStream):
    """Thread-backed non-blocking chunk stream over subprocess stdout/stderr pipes."""

    def __init__(
        self,
        proc: subprocess.Popen,
        *,
        tee_path: str = "",
    ) -> None:
        self.proc = proc
        self.path = tee_path
        self._lock = threading.Lock()
        self._event = threading.Event()
        self._out_buf: list[bytes] = []
        self._err_buf: list[bytes] = []
        self._tee_lock = threading.Lock()
        self._tee_fh = open(tee_path, "ab", buffering=0) if tee_path else None
        self._t_out = threading.Thread(target=self._pump, args=(proc.stdout, self._out_buf), daemon=True)
        self._t_err = threading.Thread(target=self._pump, args=(proc.stderr, self._err_buf), daemon=True)
        self._t_out.start()
        self._t_err.start()

    def _pump(self, stream, sink: list[bytes]) -> None:
        if stream is None:
            return
        try:
            fd = stream.fileno()
            while True:
                chunk = os.read(fd, 4096)
                if not chunk:
                    break
                with self._lock:
                    sink.append(chunk)
                if self._tee_fh is not None:
                    with self._tee_lock:
                        try:
                            self._tee_fh.write(chunk)
                        except OSError:
                            pass
                self._event.set()
        except Exception:
            pass
        finally:
            try:
                stream.close()
            except Exception:
                pass
            self._event.set()

    def read_chunks(self) -> tuple[bytes, bytes]:
        with self._lock:
            out = b"".join(self._out_buf)
            err = b"".join(self._err_buf)
            self._out_buf.clear()
            self._err_buf.clear()
            self._event.clear()
        return out, err

    def is_done(self) -> bool:
        return self.proc.poll() is not None and not self._t_out.is_alive() and not self._t_err.is_alive()

    def returncode(self) -> int | None:
        return self.proc.poll()

    def wait_event(self, timeout: float = 0.05) -> None:
        self._event.wait(timeout)

    def wait_drain(self, timeout: float = 2.0) -> tuple[bytes, bytes]:
        self._t_out.join(timeout=timeout)
        self._t_err.join(timeout=timeout)
        out, err = self.read_chunks()
        self.close()
        return out, err

    def close(self) -> None:
        with self._tee_lock:
            if self._tee_fh is not None:
                try:
                    self._tee_fh.close()
                except OSError:
                    pass
                self._tee_fh = None


class FileLogStream(BackendLogStream):
    """Offset-based incremental reader over wrapper stdout.log / stderr.log + exit.code."""

    def __init__(
        self,
        stdout_path: str,
        stderr_path: str,
        exit_path: str,
        *,
        is_alive_cb: Callable[[], bool] | None = None,
    ) -> None:
        self.path = stdout_path
        self._stdout_path = stdout_path
        self._stderr_path = stderr_path
        self._exit_path = exit_path
        self._is_alive_cb = is_alive_cb
        self._out_pos = 0
        self._err_pos = 0
        self._rc: int | None = None
        self._last_alive_check = time.monotonic()

    def _read_new(self, path: str, pos: int) -> tuple[bytes, int]:
        try:
            with open(path, "rb") as fh:
                fh.seek(pos)
                data = fh.read()
                return data, pos + len(data)
        except OSError:
            return b"", pos

    def read_chunks(self) -> tuple[bytes, bytes]:
        out, self._out_pos = self._read_new(self._stdout_path, self._out_pos)
        err, self._err_pos = self._read_new(self._stderr_path, self._err_pos)
        return out, err

    def is_done(self) -> bool:
        if self._rc is not None:
            return True
        rc = _read_int_file(self._exit_path)
        if rc is not None:
            self._rc = rc
            return True
        if self._is_alive_cb is not None:
            now = time.monotonic()
            if now - self._last_alive_check >= 0.3:
                self._last_alive_check = now
                if not self._is_alive_cb():
                    rc_retry = _read_int_file(self._exit_path)
                    self._rc = rc_retry if rc_retry is not None else -1
                    return True
        return False

    def returncode(self) -> int | None:
        if self._rc is None:
            rc = _read_int_file(self._exit_path)
            if rc is not None:
                self._rc = rc
        return self._rc

    def wait_drain(self, timeout: float = 2.0) -> tuple[bytes, bytes]:
        deadline = time.monotonic() + min(timeout, 0.5)
        while self._rc is None and time.monotonic() < deadline:
            if self.is_done():
                break
            time.sleep(0.02)
        return self.read_chunks()


class ExecBackend:
    """Minimal cross-platform execution backend contract: spawn / log / attach / cleanup / list."""

    name: str = "pipe"

    def spawn(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        task_id: str = "",
    ) -> ExecHandle:
        raise NotImplementedError

    def log(self, handle: ExecHandle) -> BackendLogStream:
        raise NotImplementedError

    def attach(self, handle: ExecHandle) -> list[str] | None:
        raise NotImplementedError

    def cleanup(self, handle: ExecHandle, *, terminate: bool = False, force: bool = False) -> None:
        raise NotImplementedError

    def list(self) -> list[dict[str, str]]:
        raise NotImplementedError


class PipeBackend(ExecBackend):
    """Fallback direct-pipe backend matching original subprocess behavior."""

    name = "pipe"

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._active: dict[str, ExecHandle] = {}

    def spawn(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        task_id: str = "",
    ) -> ExecHandle:
        popen_kwargs: dict = {
            "shell": shell,
            "cwd": cwd,
            "env": env,
            "stdin": subprocess.DEVNULL,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
        }
        if not is_windows():
            popen_kwargs["start_new_session"] = True
        else:
            popen_kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        proc = subprocess.Popen(argv, **popen_kwargs)
        sid = f"pipe-{_sanitize_task_label(task_id)}-{uuid.uuid4().hex[:6]}"
        stream = PipeLogStream(proc)
        handle = ExecHandle(
            backend=self.name,
            session_id=sid,
            argv=argv,
            pid=proc.pid,
            proc=proc,
            meta={"stream": stream},
        )
        with self._lock:
            self._active[sid] = handle
        return handle

    def log(self, handle: ExecHandle) -> BackendLogStream:
        return handle.meta["stream"]

    def attach(self, handle: ExecHandle) -> list[str] | None:
        return None

    def cleanup(self, handle: ExecHandle, *, terminate: bool = False, force: bool = False) -> None:
        proc = handle.proc
        if terminate and proc is not None and proc.poll() is None:
            _terminate_proc_or_pg(proc, proc.pid, force=force)
        if proc is not None:
            handle.returncode = proc.poll()
        stream = handle.meta.get("stream")
        if isinstance(stream, PipeLogStream) and not terminate:
            stream.close()
        with self._lock:
            self._active.pop(handle.session_id, None)

    def list(self) -> list[dict[str, str]]:
        with self._lock:
            items = list(self._active.values())
        return [
            {
                "backend": self.name,
                "session_id": h.session_id,
                "pid": str(h.pid or ""),
                "status": "running" if h.proc and h.proc.poll() is None else "exited",
            }
            for h in items
        ]


class TmuxBackend(ExecBackend):
    """POSIX/WSL2 named tmux session backend with raw wrapper capture and attach support."""

    name = "tmux"

    def __init__(
        self,
        *,
        tmux_bin: str | None = None,
        runs_dir: str | None = None,
        socket_path: str | None = None,
    ) -> None:
        self.tmux_bin = tmux_bin or shutil.which("tmux") or "tmux"
        self.runs_dir = runs_dir
        self.socket_path = socket_path

    def _tmux_cmd(self, *args: str) -> list[str]:
        if self.socket_path:
            return [self.tmux_bin, "-S", self.socket_path, *args]
        return [self.tmux_bin, *args]

    def _has_session(self, session_name: str) -> bool:
        try:
            res = subprocess.run(
                self._tmux_cmd("has-session", "-t", session_name),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=3,
            )
            return res.returncode == 0
        except Exception:
            return False

    def spawn(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        task_id: str = "",
    ) -> ExecHandle:
        runs_dir = self.runs_dir or get_runs_dir()
        safe_task = _sanitize_task_label(task_id)
        session_name = f"agenct-{safe_task}-{uuid.uuid4().hex[:6]}"
        run_dir, wrapper_path, stdout_path, stderr_path = _prepare_wrapper_run_dir(
            runs_dir, f"tmux-{safe_task}", argv, shell=shell, cwd=cwd, env=env
        )
        cmd = self._tmux_cmd("new-session", "-d", "-s", session_name, "-c", cwd, sys.executable, wrapper_path, run_dir)
        res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=10)
        if res.returncode != 0:
            shutil.rmtree(run_dir, ignore_errors=True)
            err_msg = decode_bytes(res.stderr).strip() or decode_bytes(res.stdout).strip()
            raise RuntimeError(f"tmux new-session failed ({res.returncode}): {err_msg}")
        exit_path = os.path.join(run_dir, "exit.code")
        pid_path = os.path.join(run_dir, "child.pid")
        stream = FileLogStream(
            stdout_path,
            stderr_path,
            exit_path,
            is_alive_cb=lambda: self._has_session(session_name),
        )
        return ExecHandle(
            backend=self.name,
            session_id=session_name,
            argv=argv,
            log_path=stdout_path,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
            run_dir=run_dir,
            meta={"stream": stream, "pid_path": pid_path, "exit_path": exit_path},
        )

    def log(self, handle: ExecHandle) -> BackendLogStream:
        return handle.meta["stream"]

    def attach(self, handle: ExecHandle) -> list[str] | None:
        return self._tmux_cmd("attach-session", "-t", handle.session_id)

    def cleanup(self, handle: ExecHandle, *, terminate: bool = False, force: bool = False) -> None:
        pid_path = handle.meta.get("pid_path", "")
        if terminate and pid_path:
            child_pid = _read_int_file(pid_path)
            if child_pid is None:
                deadline = time.monotonic() + 0.3
                while child_pid is None and time.monotonic() < deadline:
                    time.sleep(0.02)
                    child_pid = _read_int_file(pid_path)
            if child_pid is not None:
                _terminate_proc_or_pg(None, child_pid, force=force)
        try:
            subprocess.run(
                self._tmux_cmd("kill-session", "-t", handle.session_id),
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=5,
            )
        except Exception:
            pass
        if not terminate and handle.run_dir:
            shutil.rmtree(handle.run_dir, ignore_errors=True)

    def list(self) -> list[dict[str, str]]:
        try:
            res = subprocess.run(
                self._tmux_cmd("list-sessions", "-F", "#{session_name}\t#{session_attached}\t#{session_windows}"),
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                timeout=3,
            )
            if res.returncode != 0:
                return []
            out: list[dict[str, str]] = []
            for line in decode_bytes(res.stdout).splitlines():
                parts = line.split("\t")
                if not parts or not parts[0].startswith("agenct-"):
                    continue
                out.append(
                    {
                        "backend": self.name,
                        "session_id": parts[0],
                        "attached": parts[1] if len(parts) > 1 else "0",
                        "windows": parts[2] if len(parts) > 2 else "1",
                    }
                )
            return out
        except Exception:
            return []


class ConPtyBackend(ExecBackend):
    """Windows native ConPTY backend (pywinpty / node-pty) using wrapper capture to preserve JSONL."""

    name = "conpty"

    def __init__(
        self,
        *,
        provider: str = "pywinpty",
        runs_dir: str | None = None,
        node_bin: str | None = None,
        import_module: Callable[[str], object] | None = None,
    ) -> None:
        self.provider = provider
        self.runs_dir = runs_dir
        self.node_bin = node_bin or shutil.which("node") or "node"
        self._import_module = import_module or importlib.import_module
        self._lock = threading.Lock()
        self._active: dict[str, ExecHandle] = {}

    def spawn(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        task_id: str = "",
    ) -> ExecHandle:
        runs_dir = self.runs_dir or get_runs_dir(system="Windows")
        safe_task = _sanitize_task_label(task_id)
        session_id = f"conpty-{safe_task}-{uuid.uuid4().hex[:6]}"
        run_dir, wrapper_path, stdout_path, stderr_path = _prepare_wrapper_run_dir(
            runs_dir, f"conpty-{safe_task}", argv, shell=shell, cwd=cwd, env=env
        )
        exit_path = os.path.join(run_dir, "exit.code")
        pid_path = os.path.join(run_dir, "child.pid")
        pty_proc = None
        host_proc = None

        try:
            if self.provider == "pywinpty":
                winpty = self._import_module("winpty")
                pty_cls = getattr(winpty, "PtyProcess", None)
                if pty_cls is not None:
                    pty_proc = pty_cls.spawn([sys.executable, wrapper_path, run_dir], cwd=cwd)
                    threading.Thread(target=self._drain_pywinpty, args=(pty_proc,), daemon=True).start()
                    is_alive_cb = lambda: bool(getattr(pty_proc, "isalive", lambda: False)())
                elif hasattr(winpty, "PTY"):
                    pty_proc = winpty.PTY(120, 30)
                    pty_proc.spawn(sys.executable, cmdline=subprocess.list2cmdline([wrapper_path, run_dir]), cwd=cwd)
                    threading.Thread(target=self._drain_pywinpty, args=(pty_proc,), daemon=True).start()
                    is_alive_cb = lambda: bool(getattr(pty_proc, "isalive", lambda: False)())
                else:
                    raise RuntimeError("winpty module exposes neither PtyProcess nor PTY")
            elif self.provider == "node-pty":
                host_js = os.path.join(run_dir, "pty_host.js")
                with open(host_js, "w", encoding="utf-8") as fh:
                    fh.write(_NODE_PTY_HOST_SCRIPT)
                host_proc = subprocess.Popen(
                    [self.node_bin, host_js, sys.executable, wrapper_path, run_dir, cwd],
                    cwd=cwd,
                    env=env,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
                is_alive_cb = lambda: host_proc.poll() is None
            else:
                raise RuntimeError(f"Unsupported ConPTY provider: {self.provider}")
        except Exception:
            shutil.rmtree(run_dir, ignore_errors=True)
            raise

        stream = FileLogStream(stdout_path, stderr_path, exit_path, is_alive_cb=is_alive_cb)
        handle = ExecHandle(
            backend=self.name,
            session_id=session_id,
            argv=argv,
            log_path=stdout_path,
            stdout_path=stdout_path,
            stderr_path=stderr_path,
            proc=host_proc,
            run_dir=run_dir,
            meta={
                "stream": stream,
                "pid_path": pid_path,
                "exit_path": exit_path,
                "pty_proc": pty_proc,
                "provider": self.provider,
            },
        )
        with self._lock:
            self._active[session_id] = handle
        return handle

    @staticmethod
    def _drain_pywinpty(pty_proc: object) -> None:
        read_fn = getattr(pty_proc, "read", None)
        if not callable(read_fn):
            return
        while True:
            try:
                data = read_fn()
                if not data:
                    break
            except Exception:
                break

    def log(self, handle: ExecHandle) -> BackendLogStream:
        return handle.meta["stream"]

    def attach(self, handle: ExecHandle) -> list[str] | None:
        return None

    def cleanup(self, handle: ExecHandle, *, terminate: bool = False, force: bool = False) -> None:
        if terminate:
            child_pid = _read_int_file(handle.meta.get("pid_path", ""))
            if child_pid is not None:
                _terminate_proc_or_pg(None, child_pid, force=force)
            pty_proc = handle.meta.get("pty_proc")
            if pty_proc is not None:
                for meth in ("terminate", "kill", "close"):
                    fn = getattr(pty_proc, meth, None)
                    if callable(fn):
                        try:
                            fn(force=True) if meth == "terminate" else fn()
                            break
                        except TypeError:
                            try:
                                fn()
                                break
                            except Exception:
                                pass
                        except Exception:
                            pass
            if handle.proc is not None and handle.proc.poll() is None:
                _terminate_proc_or_pg(handle.proc, handle.proc.pid, force=force)
        if not terminate:
            pty_proc = handle.meta.get("pty_proc")
            if pty_proc is not None:
                close_fn = getattr(pty_proc, "close", None)
                if callable(close_fn):
                    try:
                        close_fn()
                    except Exception:
                        pass
            if handle.proc is not None:
                try:
                    handle.proc.wait(timeout=2)
                except Exception:
                    pass
            with self._lock:
                self._active.pop(handle.session_id, None)
            if handle.run_dir:
                shutil.rmtree(handle.run_dir, ignore_errors=True)

    def list(self) -> list[dict[str, str]]:
        with self._lock:
            items = list(self._active.values())
        return [
            {
                "backend": self.name,
                "session_id": h.session_id,
                "provider": str(h.meta.get("provider") or self.provider),
            }
            for h in items
        ]


class WtTailBackend(ExecBackend):
    """Windows lightweight tier: raw pipe execution + whitelisted Windows Terminal log-tail tab."""

    name = "wt-tail"

    def __init__(self, *, wt_bin: str = "wt.exe", runs_dir: str | None = None) -> None:
        self.wt_bin = wt_bin
        self.runs_dir = runs_dir
        self._lock = threading.Lock()
        self._active: dict[str, ExecHandle] = {}

    def spawn(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        task_id: str = "",
    ) -> ExecHandle:
        runs_dir = self.runs_dir or get_runs_dir(system="Windows")
        os.makedirs(runs_dir, exist_ok=True)
        safe_task = _sanitize_task_label(task_id)
        session_id = f"agenct:{safe_task}"
        log_path = os.path.join(runs_dir, f"agenct-{safe_task}-{uuid.uuid4().hex[:6]}.log")
        open(log_path, "ab").close()

        wt_argv = build_wt_tail_argv(self.wt_bin, safe_task, log_path)
        try:
            subprocess.Popen(
                wt_argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except OSError as exc:
            try:
                os.unlink(log_path)
            except OSError:
                pass
            raise RuntimeError(f"wt.exe launch failed: {exc}") from exc

        popen_kwargs: dict = {
            "shell": shell,
            "cwd": cwd,
            "env": env,
            "stdin": subprocess.DEVNULL,
            "stdout": subprocess.PIPE,
            "stderr": subprocess.PIPE,
        }
        if not is_windows():
            popen_kwargs["start_new_session"] = True
        else:
            popen_kwargs["creationflags"] = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
        proc = subprocess.Popen(argv, **popen_kwargs)
        stream = PipeLogStream(proc, tee_path=log_path)
        handle = ExecHandle(
            backend=self.name,
            session_id=session_id,
            argv=argv,
            log_path=log_path,
            stdout_path=log_path,
            stderr_path=log_path,
            pid=proc.pid,
            proc=proc,
            meta={"stream": stream, "wt_argv": wt_argv},
        )
        with self._lock:
            self._active[session_id] = handle
        return handle

    def log(self, handle: ExecHandle) -> BackendLogStream:
        return handle.meta["stream"]

    def attach(self, handle: ExecHandle) -> list[str] | None:
        return list(handle.meta.get("wt_argv") or []) or None

    def cleanup(self, handle: ExecHandle, *, terminate: bool = False, force: bool = False) -> None:
        proc = handle.proc
        if terminate and proc is not None and proc.poll() is None:
            _terminate_proc_or_pg(proc, proc.pid, force=force)
        if proc is not None:
            handle.returncode = proc.poll()
        stream = handle.meta.get("stream")
        if isinstance(stream, PipeLogStream) and not terminate:
            stream.close()
        with self._lock:
            self._active.pop(handle.session_id, None)

    def list(self) -> list[dict[str, str]]:
        with self._lock:
            items = list(self._active.values())
        return [
            {
                "backend": self.name,
                "session_id": h.session_id,
                "log_path": h.log_path,
                "status": "running" if h.proc and h.proc.poll() is None else "exited",
            }
            for h in items
        ]


def resolve_exec_backend(
    requested: str = "auto",
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    import_module: Callable[[str], object] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
    runs_dir: str | None = None,
    socket_path: str | None = None,
) -> ExecBackend:
    """Resolve the requested execution backend, silently degrading on probe failure without raising."""
    req = (requested or "auto").strip().lower()
    if req == "pipe":
        return PipeBackend()

    win = is_windows(system)
    if req == "auto":
        if not win:
            if probe_tmux(system=system, which=which, runner=runner, socket_path=socket_path):
                which_fn = which or shutil.which
                return TmuxBackend(tmux_bin=which_fn("tmux"), runs_dir=runs_dir, socket_path=socket_path)
            return PipeBackend()
        provider = probe_conpty(system=system, which=which, import_module=import_module, runner=runner)
        if provider:
            which_fn = which or shutil.which
            return ConPtyBackend(
                provider=provider,
                runs_dir=runs_dir,
                node_bin=which_fn("node") if provider == "node-pty" else None,
                import_module=import_module,
            )
        wt_bin = probe_wt_exe(system=system, which=which, runner=runner)
        if wt_bin:
            return WtTailBackend(wt_bin=wt_bin, runs_dir=runs_dir)
        return PipeBackend()

    if req == "tmux":
        if not win and probe_tmux(system=system, which=which, runner=runner, socket_path=socket_path):
            which_fn = which or shutil.which
            return TmuxBackend(tmux_bin=which_fn("tmux"), runs_dir=runs_dir, socket_path=socket_path)
        return PipeBackend()

    if req == "conpty":
        provider = probe_conpty(system=system, which=which, import_module=import_module, runner=runner)
        if provider:
            which_fn = which or shutil.which
            return ConPtyBackend(
                provider=provider,
                runs_dir=runs_dir,
                node_bin=which_fn("node") if provider == "node-pty" else None,
                import_module=import_module,
            )
        return PipeBackend()

    if req == "wt-tail":
        wt_bin = probe_wt_exe(system=system, which=which, runner=runner)
        if wt_bin:
            return WtTailBackend(wt_bin=wt_bin, runs_dir=runs_dir)
        return PipeBackend()

    return PipeBackend()


def run_monitored(
    argv: list[str] | str,
    *,
    shell: bool,
    cwd: str,
    env: dict[str, str],
    timeout: int,
    cancel_event: threading.Event,
    progress_cb,
    stage_label: str,
    agent_hint: str = "",
    started_at: float | None = None,
    idle_timeout: int = 0,
    backend: ExecBackend | None = None,
    task_id: str = "",
    meta_out: dict | None = None,
) -> tuple[int, bytes, bytes, bool]:
    if started_at is None:
        started_at = time.monotonic()
    active_backend: ExecBackend = backend if backend is not None else PipeBackend()
    try:
        handle = active_backend.spawn(argv, shell=shell, cwd=cwd, env=env, task_id=task_id)
    except Exception:
        if active_backend.name == "pipe":
            raise
        active_backend = PipeBackend()
        handle = active_backend.spawn(argv, shell=shell, cwd=cwd, env=env, task_id=task_id)

    if meta_out is not None:
        meta_out["backend"] = active_backend.name
        attach_cmd = active_backend.attach(handle)
        if attach_cmd:
            meta_out["attach_cmd"] = attach_cmd
        if active_backend.name == "tmux" and handle.session_id:
            meta_out["tmux_session"] = handle.session_id

    log_stream = active_backend.log(handle)
    out_chunks: list[bytes] = []
    err_chunks: list[bytes] = []
    line_bufs = [b"", b""]
    recent: list[str] = []
    last_activity = started_at
    last_idle_notice = 0.0
    last_tail = ""

    def ingest(out_delta: bytes, err_delta: bytes, *, flush_partial: bool = False) -> None:
        nonlocal last_activity, last_idle_notice, last_tail
        now = time.monotonic()
        for idx, (delta, sink) in enumerate(((out_delta, out_chunks), (err_delta, err_chunks))):
            if delta:
                sink.append(delta)
                last_activity = now
                last_idle_notice = 0.0
                line_bufs[idx] += delta
            if b"\n" in line_bufs[idx] or (flush_partial and line_bufs[idx]):
                if flush_partial:
                    lines = line_bufs[idx].splitlines(keepends=True)
                    line_bufs[idx] = b""
                else:
                    parts = line_bufs[idx].split(b"\n")
                    lines = [p + b"\n" for p in parts[:-1]]
                    line_bufs[idx] = parts[-1]
                for raw_line in lines:
                    text = summarize_progress_line(agent_hint, decode_bytes(raw_line))
                    entry = format_progress_entry(text, workdir=cwd) if text else ""
                    if entry:
                        if recent and recent[-1] == entry:
                            continue
                        recent.append(entry)
                        if len(recent) > 5:
                            recent.pop(0)
                        while len("\n".join(recent).encode("utf-8")) > 850 and len(recent) > 1:
                            recent.pop(0)
                        last_tail = "\n".join(recent)
                        progress_cb(stage_label, last_tail)

    cancelled = False
    timed_out = False
    try:
        while True:
            out_delta, err_delta = log_stream.read_chunks()
            if out_delta or err_delta:
                ingest(out_delta, err_delta)
            if log_stream.is_done():
                break
            if cancel_event.is_set():
                cancelled = True
                active_backend.cleanup(handle, terminate=True, force=False)
                break
            now = time.monotonic()
            if now - started_at >= timeout:
                timed_out = True
                active_backend.cleanup(handle, terminate=True, force=True)
                rem_out, rem_err = log_stream.wait_drain(timeout=1.0)
                ingest(rem_out, rem_err, flush_partial=True)
                raise subprocess.TimeoutExpired(argv, timeout, b"".join(out_chunks), b"".join(err_chunks))
            if idle_timeout > 0:
                idle_for = now - last_activity
                if idle_for >= idle_timeout:
                    timed_out = True
                    active_backend.cleanup(handle, terminate=True, force=True)
                    rem_out, rem_err = log_stream.wait_drain(timeout=1.0)
                    ingest(rem_out, rem_err, flush_partial=True)
                    raise IdleTimeoutExpired(argv, idle_timeout, b"".join(out_chunks), b"".join(err_chunks))
                if idle_for >= IDLE_WARN_SECS and (now - last_idle_notice) >= 15.0:
                    last_idle_notice = now
                    progress_cb(f"{stage_label}（已静默 {int(idle_for)}s）", last_tail)
            log_stream.wait_event(0.05)

        rem_out, rem_err = log_stream.wait_drain(timeout=2.0)
        ingest(rem_out, rem_err, flush_partial=True)
        rc = log_stream.returncode()
        return (rc or 0), b"".join(out_chunks), b"".join(err_chunks), cancelled
    finally:
        active_backend.cleanup(handle, terminate=False, force=False)


def sanitize_job_model(val: object) -> str:
    if not isinstance(val, str):
        return ""
    s = val.strip()
    return s if _SAFE_MODEL_RE.match(s) else ""


def sanitize_job_effort(val: object) -> str:
    if not isinstance(val, str):
        return ""
    s = val.strip().lower()
    return s if s in VALID_EFFORTS else ""


def default_masked_paths(
    home_dir: str | None = None,
    state_dir: str | None = None,
) -> tuple[str, ...]:
    """Return host credential and node state paths hidden inside OS sandboxes."""
    home = os.path.abspath(home_dir or os.path.expanduser("~"))
    out: list[str] = [os.path.join(home, name) for name in _DEFAULT_MASKED_SECRET_NAMES]
    if state_dir:
        out.append(os.path.abspath(state_dir))
    return tuple(out)


class SandboxPolicy:
    """Engine-agnostic declarative isolation policy enforced outside the CLI."""

    __slots__ = (
        "mode",
        "workdir",
        "evidence_dir",
        "in_dir",
        "out_file",
        "state_paths",
        "masked_paths",
    )

    def __init__(
        self,
        *,
        mode: str,
        workdir: str,
        evidence_dir: str = "",
        in_dir: str | None = None,
        out_file: str | None = None,
        state_paths: tuple[str, ...] = (),
        masked_paths: tuple[str, ...] = (),
    ) -> None:
        self.mode = mode if mode in _SANDBOX_RANK else "workspace-write"
        self.workdir = workdir
        self.evidence_dir = evidence_dir
        self.in_dir = in_dir
        self.out_file = out_file
        self.state_paths = state_paths
        self.masked_paths = masked_paths


class SandboxBackend:
    """Abstract OS-level sandbox wrapper parallel to ExecBackend."""

    name: str = "cli-native"
    os_isolated: bool = False

    def wrap(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        policy: SandboxPolicy,
    ) -> tuple[list[str] | str, bool, str, dict[str, str]]:
        return argv, shell, cwd, dict(env)


def probe_bwrap(
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
) -> bool:
    """Return True when bubblewrap can create unprivileged mount + PID namespaces."""
    sys_name = (platform.system() if system is None else system).strip().lower()
    if sys_name != "linux":
        return False
    which_fn = which or shutil.which
    bwrap_bin = which_fn("bwrap")
    if not bwrap_bin:
        return False
    run_fn = runner or subprocess.run
    try:
        res = run_fn(
            [
                bwrap_bin,
                "--ro-bind", "/", "/",
                "--dev", "/dev",
                "--proc", "/proc",
                "--tmpfs", "/tmp",
                "--unshare-pid",
                "--die-with-parent",
                "--",
                "/bin/true",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=3,
        )
        return res.returncode == 0
    except Exception:
        return False


def probe_seatbelt(
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
) -> bool:
    """Return True when macOS sandbox-exec (Seatbelt) is installed and permitted."""
    sys_name = (platform.system() if system is None else system).strip().lower()
    if sys_name != "darwin":
        return False
    which_fn = which or shutil.which
    sb_bin = which_fn("sandbox-exec")
    if not sb_bin:
        return False
    run_fn = runner or subprocess.run
    try:
        res = run_fn(
            [sb_bin, "-p", "(version 1)(allow default)", "/usr/bin/true"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=3,
        )
        return res.returncode == 0
    except Exception:
        return False


def _is_same_or_parent_dir(candidate: str, target: str) -> bool:
    return target == candidate or target.startswith(candidate + os.sep)


class BubblewrapSandbox(SandboxBackend):
    """Linux/WSL2 kernel mount + PID namespace sandbox via bubblewrap."""

    name = "bwrap"
    os_isolated = True

    def __init__(self, bwrap_bin: str | None = None) -> None:
        self.bwrap_bin = bwrap_bin or shutil.which("bwrap") or "bwrap"

    def wrap(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        policy: SandboxPolicy,
    ) -> tuple[list[str] | str, bool, str, dict[str, str]]:
        out_env = dict(env)
        if policy.mode == "unsafe-full":
            return argv, shell, cwd, out_env

        cmd: list[str] = [
            self.bwrap_bin,
            "--ro-bind", "/", "/",
            "--dev", "/dev",
            "--proc", "/proc",
            "--tmpfs", "/tmp",
        ]
        workdir_real = (
            os.path.realpath(policy.workdir)
            if policy.workdir and os.path.exists(policy.workdir)
            else ""
        )

        # 1. Mask sensitive credential/state paths before overlaying workspace/task mounts.
        # Broken symlinks (e.g. ~/.aws -> /mnt/c/...) return False for os.path.exists and are skipped safely.
        masked_real: set[str] = set()
        for raw_p in policy.masked_paths:
            if not raw_p or not os.path.exists(raw_p):
                continue
            real = os.path.realpath(raw_p)
            if workdir_real and _is_same_or_parent_dir(real, workdir_real):
                continue
            if real in masked_real:
                continue
            if os.path.isdir(real):
                cmd.extend(["--tmpfs", real])
                masked_real.add(real)
            elif os.path.isfile(real):
                cmd.extend(["--ro-bind", "/dev/null", real])
                masked_real.add(real)

        bound_real: set[str] = set()
        # 2. Workspace mount: writable only in workspace-write, explicit ro-bind in readonly
        # so temp workdirs under /tmp remain readable even after --tmpfs /tmp.
        if workdir_real and os.path.isdir(workdir_real):
            flag = "--bind" if policy.mode == "workspace-write" else "--ro-bind"
            cmd.extend([flag, workdir_real, workdir_real])
            bound_real.add(workdir_real)

        def _overlay_bind(raw_path: str | None, *, ro: bool = False) -> None:
            if not raw_path or not os.path.exists(raw_path):
                return
            real = os.path.realpath(raw_path)
            if any(_is_same_or_parent_dir(m, real) for m in masked_real) or real in bound_real:
                return
            cmd.extend(["--ro-bind" if ro else "--bind", real, real])
            bound_real.add(real)

        # 3. Task I/O mounts (must come after --tmpfs /tmp so evidence/inputs/output remain visible).
        _overlay_bind(policy.evidence_dir, ro=False)
        _overlay_bind(policy.in_dir, ro=True)
        _overlay_bind(policy.out_file, ro=False)

        # 4. Engine session/cache state paths required for headless execution & session resume.
        for state_p in policy.state_paths:
            _overlay_bind(state_p, ro=False)

        cmd.extend(["--unshare-pid", "--die-with-parent", "--chdir", cwd, "--"])
        if shell:
            cmd.extend(["/bin/sh", "-c", str(argv)])
        else:
            cmd.extend(list(argv) if not isinstance(argv, str) else [argv])
        return cmd, False, cwd, out_env


def _sbpl_escape(path: str) -> str:
    return path.replace("\\", "\\\\").replace('"', '\\"')


def _sbpl_path_filter(path: str) -> str:
    escaped = _sbpl_escape(path)
    return f'(subpath "{escaped}")' if os.path.isdir(path) else f'(literal "{escaped}")'


class SeatbeltSandbox(SandboxBackend):
    """macOS sandbox-exec (Seatbelt SBPL) filesystem isolation backend."""

    name = "seatbelt"
    os_isolated = True

    def __init__(self, sandbox_exec_bin: str | None = None) -> None:
        self.sandbox_exec_bin = sandbox_exec_bin or shutil.which("sandbox-exec") or "sandbox-exec"

    def build_profile(self, policy: SandboxPolicy) -> str:
        lines = ["(version 1)", "(allow default)", "(deny file-write*)"]
        for tmp_dir in ("/tmp", "/private/tmp", "/private/var/folders"):
            lines.append(f'(allow file-write* (subpath "{_sbpl_escape(tmp_dir)}"))')
        for dev_path in ("/dev/null", "/dev/tty", "/dev/dtracehelper"):
            lines.append(f'(allow file-write* (literal "{_sbpl_escape(dev_path)}"))')

        workdir_real = (
            os.path.realpath(policy.workdir)
            if policy.workdir and os.path.exists(policy.workdir)
            else policy.workdir
        )
        masked_candidates: list[str] = []
        masked_real: set[str] = set()
        for raw_p in policy.masked_paths:
            if not raw_p or not os.path.exists(raw_p):
                continue
            real = os.path.realpath(raw_p)
            if workdir_real and _is_same_or_parent_dir(real, workdir_real):
                continue
            if real not in masked_real:
                masked_real.add(real)
                masked_candidates.append(real)
                if os.path.abspath(raw_p) != real:
                    masked_candidates.append(os.path.abspath(raw_p))

        def _allow_write(raw_p: str | None) -> None:
            if not raw_p or not os.path.exists(raw_p):
                return
            for candidate in dict.fromkeys((os.path.abspath(raw_p), os.path.realpath(raw_p))):
                if any(_is_same_or_parent_dir(m, candidate) for m in masked_real):
                    continue
                lines.append(f"(allow file-write* {_sbpl_path_filter(candidate)})")

        if policy.mode == "workspace-write" and policy.workdir:
            _allow_write(policy.workdir)
        _allow_write(policy.evidence_dir)
        _allow_write(policy.out_file)
        for sp in policy.state_paths:
            _allow_write(sp)

        for mp in masked_candidates:
            lines.append(f"(deny file-read* file-write* {_sbpl_path_filter(mp)})")
        return "\n".join(lines)

    def wrap(
        self,
        argv: list[str] | str,
        *,
        shell: bool,
        cwd: str,
        env: dict[str, str],
        policy: SandboxPolicy,
    ) -> tuple[list[str] | str, bool, str, dict[str, str]]:
        out_env = dict(env)
        if policy.mode == "unsafe-full":
            return argv, shell, cwd, out_env
        cmd: list[str] = [self.sandbox_exec_bin, "-p", self.build_profile(policy)]
        if shell:
            cmd.extend(["/bin/sh", "-c", str(argv)])
        else:
            cmd.extend(list(argv) if not isinstance(argv, str) else [argv])
        return cmd, False, cwd, out_env


class CliNativeSandbox(SandboxBackend):
    """Honest fallback when host OS provides no kernel isolation (delegates to CLI flags)."""

    name = "cli-native"
    os_isolated = False


class DirectSandbox(SandboxBackend):
    """No outer isolation wrapper (used exclusively for unsafe-full / manual elevated runs)."""

    name = "direct"
    os_isolated = False


def resolve_sandbox_backend(
    requested: str | None = "auto",
    *,
    system: str | None = None,
    which: Callable[[str], str | None] | None = None,
    runner: Callable[..., subprocess.CompletedProcess] | None = None,
) -> SandboxBackend:
    """Resolve host OS sandbox backend (auto -> bwrap | seatbelt | cli-native)."""
    raw = (requested or "auto").strip().lower()
    if raw not in VALID_SANDBOX_BACKENDS:
        print(
            f"[agenct-node] Warning: unknown sandbox backend '{raw}'; falling back to 'auto'.",
            file=sys.stderr,
            flush=True,
        )
        raw = "auto"

    which_fn = which or shutil.which
    if raw == "cli-native":
        return CliNativeSandbox()
    if raw == "bwrap":
        if probe_bwrap(system=system, which=which_fn, runner=runner):
            return BubblewrapSandbox(bwrap_bin=which_fn("bwrap"))
        print(
            "[agenct-node] Warning: --sandbox-backend=bwrap unavailable on this host; falling back to 'cli-native'.",
            file=sys.stderr,
            flush=True,
        )
        return CliNativeSandbox()
    if raw == "seatbelt":
        if probe_seatbelt(system=system, which=which_fn, runner=runner):
            return SeatbeltSandbox(sandbox_exec_bin=which_fn("sandbox-exec"))
        print(
            "[agenct-node] Warning: --sandbox-backend=seatbelt unavailable on this host; falling back to 'cli-native'.",
            file=sys.stderr,
            flush=True,
        )
        return CliNativeSandbox()

    if probe_bwrap(system=system, which=which_fn, runner=runner):
        return BubblewrapSandbox(bwrap_bin=which_fn("bwrap"))
    if probe_seatbelt(system=system, which=which_fn, runner=runner):
        return SeatbeltSandbox(sandbox_exec_bin=which_fn("sandbox-exec"))
    return CliNativeSandbox()


def tighten_sandbox(node_sandbox: str, requested: object) -> str:
    """Downward-only sandbox override: can tighten (e.g. -> readonly), never escalate."""
    base = node_sandbox if node_sandbox in _SANDBOX_RANK else "workspace-write"
    if not isinstance(requested, str):
        return base
    req = requested.strip().lower()
    if req == "read-only":
        req = "readonly"
    if req not in ("readonly", "workspace-write"):
        return base
    return req if _SANDBOX_RANK[req] < _SANDBOX_RANK[base] else base


def resolve_job_sandbox(node_sandbox: str, job: dict, *, allow_elevated: bool = False) -> str:
    """Resolve effective sandbox: downward-only unless host opted in AND manual retry requested elevated."""
    req = str(job.get("sandbox") or "").strip().lower()
    auth = str(job.get("authorization") or "").strip().lower()
    if allow_elevated and req == "elevated" and auth == "manual":
        return "unsafe-full"
    return tighten_sandbox(node_sandbox, job.get("sandbox"))


class CliAdapter:
    """Explicit headless contract that every supported CLI engine must satisfy."""

    __slots__ = (
        "name",
        "readonly_disables_shell",
        "state_paths",
        "build_argv",
        "build_env",
        "parse_stream",
        "summarize_event",
    )

    def __init__(
        self,
        *,
        name: str,
        readonly_disables_shell: bool,
        state_paths: tuple[str, ...],
        build_argv: Callable[..., list[str]],
        build_env: Callable[..., dict[str, str]],
        parse_stream: Callable[[str], tuple[str | None, str | None, bool]],
        summarize_event: Callable[[dict], str],
    ) -> None:
        self.name = name
        self.readonly_disables_shell = readonly_disables_shell
        self.state_paths = state_paths
        self.build_argv = build_argv
        self.build_env = build_env
        self.parse_stream = parse_stream
        self.summarize_event = summarize_event


def _default_cli_env(
    base_env: dict[str, str],
    _sandbox: str,
    *,
    os_isolated: bool = False,
) -> dict[str, str]:
    _ = os_isolated
    return dict(base_env)


def _opencode_cli_env(
    base_env: dict[str, str],
    sandbox: str,
    *,
    os_isolated: bool = False,
) -> dict[str, str]:
    env = dict(base_env)
    if os_isolated or sandbox == "unsafe-full":
        env.pop("OPENCODE_PERMISSION", None)
    elif sandbox == "readonly":
        env["OPENCODE_PERMISSION"] = _OPENCODE_READONLY_PERMISSION
    else:
        env["OPENCODE_PERMISSION"] = _OPENCODE_WORKSPACE_PERMISSION
    return env


def _build_claude_argv(
    exe: str,
    prompt: str,
    workdir: str,
    sandbox: str,
    evidence_dir: str,
    in_dir: str | None,
    _out_file: str | None = None,
    resume_sid: str = "",
    model: str = "",
    effort: str = "",
    *,
    os_isolated: bool = False,
) -> list[str]:
    add_dirs = ["--add-dir", workdir, "--add-dir", evidence_dir]
    if in_dir:
        add_dirs.extend(["--add-dir", in_dir])
    if sandbox == "unsafe-full":
        add_dirs.extend(["--add-dir", os.path.expanduser("~")])
    argv = [exe, "-p", "--output-format", "stream-json", "--verbose"]
    if model:
        argv.extend(["--model", model])
    if effort:
        cl_effort = "low" if effort == "minimal" else effort
        argv.extend(["--effort", cl_effort])
    if resume_sid:
        argv.extend(["--resume", resume_sid])
    if os_isolated or sandbox == "unsafe-full":
        argv.append("--dangerously-skip-permissions")
    elif sandbox == "readonly":
        argv.extend(["--allowedTools", "Read,Glob,Grep", "--disallowedTools", "Bash,Edit,Write,NotebookEdit"])
    else:
        argv.extend(["--allowedTools", "Bash,Read,Edit,Write,Glob,Grep"])
    argv.extend([*add_dirs, "--", prompt])
    return argv


def _build_codex_argv(
    exe: str,
    prompt: str,
    workdir: str,
    sandbox: str,
    evidence_dir: str,
    in_dir: str | None,
    out_file: str | None = None,
    resume_sid: str = "",
    model: str = "",
    effort: str = "",
    codex_tmux: bool = False,
    *,
    os_isolated: bool = False,
) -> list[str]:
    model_args = ["-m", model] if model else []
    eff_args = (
        ["-c", f'model_reasoning_effort="{"xhigh" if effort == "max" else effort}"']
        if effort
        else []
    )
    out_args = ["-o", out_file] if out_file else []
    if os_isolated:
        if resume_sid:
            return [
                exe, "exec", "resume",
                *model_args,
                "--dangerously-bypass-approvals-and-sandbox",
                *eff_args,
                "--skip-git-repo-check", "--json", *out_args, resume_sid, prompt,
            ]
        extra = ["--add-dir", evidence_dir]
        if in_dir:
            extra.extend(["--add-dir", in_dir])
        return [
            exe, "exec", "--skip-git-repo-check", "--json",
            *model_args,
            "--dangerously-bypass-approvals-and-sandbox", *eff_args, "-C", workdir, *extra, *out_args, prompt,
        ]

    sb_mode = {
        "readonly": "read-only",
        "workspace-write": "workspace-write",
        "unsafe-full": "danger-full-access",
    }.get(sandbox, "workspace-write")
    # Fallback when OS isolation is unavailable: rely on Codex's native Landlock/Seatbelt sandbox.
    if sb_mode == "workspace-write":
        allow_network = sandbox == "workspace-write" and bool(codex_tmux)
        extra_cfg = ["-c", f"sandbox_workspace_write.network_access={'true' if allow_network else 'false'}"]
    else:
        extra_cfg = []
    if resume_sid:
        return [
            exe, "exec", "resume",
            *model_args,
            "-c", f'sandbox_mode="{sb_mode}"',
            *extra_cfg,
            *eff_args,
            "--skip-git-repo-check", "--json", *out_args, resume_sid, prompt,
        ]
    extra = ["--add-dir", evidence_dir]
    if in_dir:
        extra.extend(["--add-dir", in_dir])
    return [
        exe, "exec", "--skip-git-repo-check", "--json",
        *model_args,
        "-s", sb_mode, *extra_cfg, *eff_args, "-C", workdir, *extra, *out_args, prompt,
    ]


def _build_agy_argv(
    exe: str,
    prompt: str,
    workdir: str,
    sandbox: str,
    evidence_dir: str,
    in_dir: str | None,
    _out_file: str | None = None,
    resume_sid: str = "",
    model: str = "",
    effort: str = "",
    *,
    os_isolated: bool = False,
) -> list[str]:
    argv = [exe, "-p", prompt, "--output-format", "stream-json"]
    if model:
        argv.extend(["--model", model])
    if effort:
        ag_effort = "low" if effort == "minimal" else effort
        argv.extend(["--effort", ag_effort])
    if os_isolated or sandbox == "unsafe-full":
        argv.append("--dangerously-skip-permissions")
        argv.extend(["--add-dir", workdir, "--add-dir", evidence_dir])
        if in_dir:
            argv.extend(["--add-dir", in_dir])
    elif sandbox == "readonly":
        argv.extend(["--sandbox", "--settings", _build_agy_readonly_settings(workdir, in_dir)])
    else:
        argv.extend(["--mode", "accept-edits", "--sandbox", "--settings", _AGY_WORKSPACE_SETTINGS])
        argv.extend(["--add-dir", workdir, "--add-dir", evidence_dir])
        if in_dir:
            argv.extend(["--add-dir", in_dir])
    if resume_sid:
        argv.extend(["--conversation", resume_sid])
    return argv


def _build_opencode_argv(
    exe: str,
    prompt: str,
    _workdir: str,
    sandbox: str,
    _evidence_dir: str,
    _in_dir: str | None,
    _out_file: str | None = None,
    resume_sid: str = "",
    model: str = "",
    effort: str = "",
    *,
    os_isolated: bool = False,
) -> list[str]:
    argv = [exe, "run", "--format", "json"]
    if os_isolated or sandbox == "unsafe-full":
        argv.append("--auto")
    if model:
        argv.extend(["-m", model])
    if effort:
        oc_effort = "high" if effort == "xhigh" else effort
        argv.extend(["--variant", oc_effort])
    if resume_sid:
        argv.extend(["--session", resume_sid])
    argv.append(prompt)
    return argv


CLI_ADAPTERS: dict[str, CliAdapter] = {
    "claude": CliAdapter(
        name="claude",
        readonly_disables_shell=True,
        state_paths=(".claude", ".claude.json"),
        build_argv=_build_claude_argv,
        build_env=_default_cli_env,
        parse_stream=_parse_claude_stream,
        summarize_event=_summarize_claude_event,
    ),
    "codex": CliAdapter(
        name="codex",
        readonly_disables_shell=False,
        state_paths=(".codex",),
        build_argv=_build_codex_argv,
        build_env=_default_cli_env,
        parse_stream=_parse_codex_stream,
        summarize_event=_summarize_codex_event,
    ),
    "agy": CliAdapter(
        name="agy",
        readonly_disables_shell=False,
        state_paths=(".gemini", ".config/antigravity"),
        build_argv=_build_agy_argv,
        build_env=_default_cli_env,
        parse_stream=_parse_agy_stream,
        summarize_event=_summarize_agy_event,
    ),
    "opencode": CliAdapter(
        name="opencode",
        readonly_disables_shell=True,
        state_paths=(".local/share/opencode", ".cache/opencode", ".local/state/opencode", ".config/opencode"),
        build_argv=_build_opencode_argv,
        build_env=_opencode_cli_env,
        parse_stream=_parse_opencode_stream,
        summarize_event=_summarize_opencode_event,
    ),
}


def build_cli_env(
    agent: str,
    base_env: dict[str, str],
    sandbox: str,
    *,
    os_isolated: bool = False,
) -> dict[str, str]:
    adapter = CLI_ADAPTERS.get(agent)
    if adapter:
        return adapter.build_env(base_env, sandbox, os_isolated=os_isolated)
    return dict(base_env)


def build_cli_argv(
    agent: str,
    exe: str,
    prompt: str,
    workdir: str,
    sandbox: str,
    evidence_dir: str,
    in_dir: str | None,
    out_file: str | None = None,
    resume_sid: str = "",
    model: str = "",
    effort: str = "",
    codex_tmux: bool = False,
    *,
    os_isolated: bool = False,
) -> list[str]:
    adapter = CLI_ADAPTERS.get(agent)
    if not adapter:
        raise ValueError(f"Unsupported CLI agent: {agent}")
    if agent == "codex":
        return adapter.build_argv(
            exe, prompt, workdir, sandbox, evidence_dir, in_dir, out_file, resume_sid, model, effort,
            codex_tmux=codex_tmux, os_isolated=os_isolated,
        )
    return adapter.build_argv(
        exe, prompt, workdir, sandbox, evidence_dir, in_dir, out_file, resume_sid, model, effort,
        os_isolated=os_isolated,
    )


def execute_job(
    agent: str,
    job: dict,
    workdir: str,
    sandbox: str,
    timeout: int,
    cancel_event: threading.Event,
    progress_cb,
    meta_out: dict | None = None,
    idle_timeout: int = DEFAULT_IDLE_TIMEOUT,
    codex_tmux: bool = False,
    exec_backend: str | ExecBackend = "auto",
    allow_elevated: bool = False,
    sandbox_backend: str | SandboxBackend = "auto",
    state_dir: str | None = None,
) -> tuple[str, str, list[dict]]:
    eff_sandbox = resolve_job_sandbox(sandbox, job, allow_elevated=allow_elevated)
    if eff_sandbox == "unsafe-full":
        resolved_sb: SandboxBackend = DirectSandbox()
    else:
        resolved_sb = (
            sandbox_backend
            if isinstance(sandbox_backend, SandboxBackend)
            else resolve_sandbox_backend(sandbox_backend)
        )
    os_isolated = resolved_sb.os_isolated and eff_sandbox != "unsafe-full"
    eff_codex_tmux = bool(codex_tmux) and eff_sandbox == "workspace-write" and not os_isolated
    resolved_backend = (
        exec_backend if isinstance(exec_backend, ExecBackend) else resolve_exec_backend(exec_backend)
    )
    if meta_out is not None:
        meta_out["backend"] = resolved_backend.name
        meta_out["isolation"] = resolved_sb.name
    task_id_str = str(job.get("task_id") or "")
    job_model = sanitize_job_model(job.get("model"))
    job_effort = sanitize_job_effort(job.get("effort"))
    job_role = str(job.get("role") or "").strip()[:32]
    input_files, in_dir = materialize_attachments(job.get("attachments") or [], workdir, eff_sandbox)
    evidence_dir = make_scoped_tempdir("agenct-ev-", workdir, eff_sandbox)
    resume_sid = ""
    home_dir = os.path.expanduser("~")
    adapter = CLI_ADAPTERS.get(agent)
    state_paths = (
        tuple(os.path.join(home_dir, rel) for rel in adapter.state_paths)
        if adapter
        else ()
    )
    masked_paths = default_masked_paths(home_dir=home_dir, state_dir=state_dir)
    try:
        prompt = build_prompt(
            job,
            input_files,
            evidence_dir,
            workdir,
            sandbox=eff_sandbox,
            agent=agent,
            os_isolated=os_isolated,
        )
        env = _build_child_base_env(sandbox=eff_sandbox)
        env["AGENCT_WORKDIR"] = workdir
        env["AGENCT_EVIDENCE_DIR"] = evidence_dir
        env["AGENCT_PROMPT"] = prompt
        env["AGENCT_ROLE"] = job_role
        env["AGENCT_MODEL"] = job_model
        env["AGENCT_EFFORT"] = job_effort
        env["AGENCT_SANDBOX"] = eff_sandbox
        env["AGENCT_ISOLATION"] = resolved_sb.name
        env["NO_COLOR"] = "1"
        env["CLICOLOR"] = "0"
        env["FORCE_COLOR"] = "0"
        env["TERM"] = "dumb"

        if agent.startswith("custom:"):
            custom_cmd = agent[len("custom:"):]
            sb_policy = SandboxPolicy(
                mode=eff_sandbox,
                workdir=workdir,
                evidence_dir=evidence_dir,
                in_dir=in_dir,
                state_paths=(),
                masked_paths=masked_paths,
            )
            wrapped_argv, wrapped_shell, wrapped_cwd, wrapped_env = resolved_sb.wrap(
                custom_cmd,
                shell=True,
                cwd=workdir,
                env=env,
                policy=sb_policy,
            )
            rc, out, err, cancelled = run_monitored(
                wrapped_argv,
                shell=wrapped_shell,
                cwd=wrapped_cwd,
                env=wrapped_env,
                timeout=timeout,
                cancel_event=cancel_event,
                progress_cb=progress_cb,
                stage_label="Running custom command",
                agent_hint="custom",
                idle_timeout=idle_timeout,
                backend=resolved_backend,
                task_id=task_id_str,
                meta_out=meta_out,
            )
            if cancelled:
                return "failed", "Job cancelled because Hub lease expired or was revoked.", []
            raw_out_s = decode_bytes(out)
            raw_err_s = decode_bytes(err)
            text = strip_ansi(raw_out_s).strip() or strip_ansi(raw_err_s).strip()
            status = "completed" if rc == 0 and text else "failed"
            if status == "failed" and meta_out is not None:
                blocked = detect_sandbox_block("custom", raw_out_s, raw_err_s, text)
                if blocked:
                    meta_out["blocked_reason"] = blocked
            return status, truncate_utf8(text or f"Command exited with code {rc}"), collect_evidence(evidence_dir)

        if agent in CLI_AGENTS:
            exe = shutil.which(agent)
            if not exe:
                return "failed", f"Agent CLI '{agent}' not found in PATH.", []
            if agent == "opencode" and job_model and "/" not in job_model:
                return (
                    "failed",
                    f"Invalid opencode model '{job_model}': opencode requires 'provider/model' format (e.g. deepseek/deepseek-flash).",
                    [],
                )
            out_file = None
            ro_cwd: str | None = None
            try:
                if agent == "codex":
                    fd, out_file = tempfile.mkstemp(prefix="agenct-codex-", suffix=".txt")
                    os.close(fd)
                if not os_isolated and agent == "agy" and eff_sandbox == "readonly":
                    ro_cwd = tempfile.mkdtemp(prefix="agenct-ro-")
                    os.chmod(ro_cwd, 0o700)
                cli_cwd = ro_cwd or workdir
                resume_sid = _valid_session_id(job.get("agent_session_id")) or ""
                role_prefix = f"{job_role}/" if job_role else ""
                model_suffix = f" ({job_model})" if job_model else ""
                stage_label = f"Running {role_prefix}{agent}{model_suffix}"
                argv = build_cli_argv(
                    agent, exe, prompt, workdir, eff_sandbox, evidence_dir, in_dir, out_file, resume_sid,
                    model=job_model, effort=job_effort, codex_tmux=eff_codex_tmux, os_isolated=os_isolated,
                )
                cli_env = build_cli_env(agent, env, eff_sandbox, os_isolated=os_isolated)
                sb_policy = SandboxPolicy(
                    mode=eff_sandbox,
                    workdir=workdir,
                    evidence_dir=evidence_dir,
                    in_dir=in_dir,
                    out_file=out_file,
                    state_paths=state_paths,
                    masked_paths=masked_paths,
                )
                wrapped_argv, wrapped_shell, wrapped_cwd, wrapped_env = resolved_sb.wrap(
                    argv,
                    shell=False,
                    cwd=cli_cwd,
                    env=cli_env,
                    policy=sb_policy,
                )

                started_at = time.monotonic()
                rc, out, err, cancelled = run_monitored(
                    wrapped_argv,
                    shell=wrapped_shell,
                    cwd=wrapped_cwd,
                    env=wrapped_env,
                    timeout=timeout,
                    cancel_event=cancel_event,
                    progress_cb=progress_cb,
                    stage_label=stage_label,
                    agent_hint=agent,
                    started_at=started_at,
                    idle_timeout=idle_timeout,
                    backend=resolved_backend,
                    task_id=task_id_str,
                    meta_out=meta_out,
                )
                if cancelled:
                    return "failed", "Job cancelled because Hub lease expired or was revoked.", []
                # If resuming a CLI session failed or produced empty output, fall back cleanly to a fresh session.
                if resume_sid:
                    probe_file = ""
                    if out_file and os.path.exists(out_file):
                        with open(out_file, "rb") as fh:
                            probe_file = strip_ansi(decode_bytes(fh.read())).strip()
                    raw_probe = decode_bytes(out)
                    probe_parsed, _ = parse_cli_output(agent, raw_probe)
                    probe_text = probe_file or (
                        probe_parsed if probe_parsed is not None else strip_ansi(raw_probe).strip()
                    )
                    if rc != 0 or not probe_text.strip():
                        if time.monotonic() - started_at >= timeout:
                            raise subprocess.TimeoutExpired(wrapped_argv, timeout, out, err)
                        resume_sid = ""
                        if out_file and os.path.exists(out_file):
                            open(out_file, "wb").close()
                        fresh_argv = build_cli_argv(
                            agent, exe, prompt, workdir, eff_sandbox, evidence_dir, in_dir, out_file, "",
                            model=job_model, effort=job_effort, codex_tmux=eff_codex_tmux, os_isolated=os_isolated,
                        )
                        wrapped_fresh_argv, wrapped_fresh_shell, wrapped_fresh_cwd, wrapped_fresh_env = resolved_sb.wrap(
                            fresh_argv,
                            shell=False,
                            cwd=cli_cwd,
                            env=cli_env,
                            policy=sb_policy,
                        )
                        rc, out, err, cancelled = run_monitored(
                            wrapped_fresh_argv,
                            shell=wrapped_fresh_shell,
                            cwd=wrapped_fresh_cwd,
                            env=wrapped_fresh_env,
                            timeout=timeout,
                            cancel_event=cancel_event,
                            progress_cb=progress_cb,
                            stage_label=stage_label,
                            agent_hint=agent,
                            started_at=started_at,
                            idle_timeout=idle_timeout,
                            backend=resolved_backend,
                            task_id=task_id_str,
                            meta_out=meta_out,
                        )
                        if cancelled:
                            return "failed", "Job cancelled because Hub lease expired or was revoked.", []
                file_text = ""
                if out_file and os.path.exists(out_file):
                    with open(out_file, "rb") as fh:
                        file_text = strip_ansi(decode_bytes(fh.read())).strip()
                raw_out = decode_bytes(out)
                raw_err = decode_bytes(err)
                parsed, captured_sid = parse_cli_output(agent, raw_out)
                if captured_sid and meta_out is not None:
                    meta_out["agent_session_id"] = captured_sid
                out_text = parsed if parsed is not None else strip_ansi(raw_out).strip()
                text = file_text or out_text or strip_ansi(raw_err).strip()
                stream_err = detect_cli_stream_error(agent, raw_out, raw_err)
                status = "completed" if rc == 0 and not stream_err and text else "failed"
                if status == "failed" and meta_out is not None:
                    blocked = detect_sandbox_block(agent, raw_out, raw_err, text)
                    if blocked:
                        meta_out["blocked_reason"] = blocked
                return status, truncate_utf8(text or f"{agent} exited with code {rc}"), collect_evidence(evidence_dir)
            finally:
                if ro_cwd:
                    shutil.rmtree(ro_cwd, ignore_errors=True)
                if out_file and os.path.exists(out_file):
                    try:
                        os.unlink(out_file)
                    except OSError:
                        pass

        return "failed", f"Unsupported execution engine: {agent}", []
    except subprocess.TimeoutExpired as exc:
        raw_out = decode_bytes(exc.stdout)
        _, captured_sid = parse_cli_output(agent, raw_out)
        session_id = captured_sid or resume_sid
        if isinstance(exc, IdleTimeoutExpired):
            content = (
                f"[idle-timeout] No output for {idle_timeout}s; terminated hung process to unblock queue. "
                f"Adjust {NODE_IDLE_TIMEOUT_ENV} or --idle-timeout (0 disables)."
            )
        else:
            content = f"[timeout] Execution timed out after {timeout}s. Adjust {NODE_TIMEOUT_ENV} or --timeout."
        if session_id:
            if meta_out is not None:
                meta_out["agent_session_id"] = session_id
            content += f"\nAgent session: {session_id}"
        tmux_session = (
            job.get("tmux_session")
            or job.get("tmux_session_name")
            or ((meta_out or {}).get("tmux_session") if isinstance(meta_out, dict) else None)
        )
        if isinstance(tmux_session, str) and tmux_session.strip():
            content += f"\nTmux session: {strip_ansi(tmux_session).strip()[:128]}"
        streams = [
            (f"\n\n{label}:\n", output)
            for label, raw in (("stdout", raw_out), ("stderr", decode_bytes(exc.stderr)))
            if (output := strip_ansi(raw).strip())
        ]
        if streams:
            budget = MAX_RESULT_BYTES - len(content.encode("utf-8")) - sum(len(header) for header, _ in streams)
            for header, output in streams:
                content += header + truncate_utf8(output, budget // len(streams))
        return "timeout", content, collect_evidence(evidence_dir)
    except Exception as exc:
        return "failed", f"Execution error: {type(exc).__name__}", []
    finally:
        if in_dir:
            shutil.rmtree(in_dir, ignore_errors=True)
        shutil.rmtree(evidence_dir, ignore_errors=True)


def run_node_loop(args: argparse.Namespace) -> int:
    hub = args.hub.rstrip("/")
    token = args.token or os.environ.get("AGENCT_NODE_TOKEN", "").strip()
    if not token:
        print("Error: AGENCT_NODE_TOKEN or --token is required.", file=sys.stderr)
        return 2
    workdir = os.path.abspath(args.workdir or os.environ.get("AGENCT_WORKDIR") or os.getcwd())
    if not os.path.isdir(workdir):
        print(f"Error: workdir does not exist: {workdir}", file=sys.stderr)
        return 2
    sandbox = "unsafe-full" if args.unsafe_full_access else args.sandbox
    allow_elevated = bool(getattr(args, "allow_elevated", False))
    codex_tmux = bool(args.codex_tmux and sandbox == "workspace-write")
    if args.codex_tmux and not codex_tmux:
        print(
            f"[agenct-node] Warning: --codex-tmux only takes effect under sandbox=workspace-write "
            f"(current: {sandbox}); ignoring.",
            file=sys.stderr,
            flush=True,
        )
    exec_backend_spec = getattr(args, "exec_backend", "auto")
    initial_backend = resolve_exec_backend(exec_backend_spec)
    sandbox_backend_spec = getattr(args, "sandbox_backend", "auto")
    os_sandbox_backend = resolve_sandbox_backend(sandbox_backend_spec)
    node_isolation = "direct" if sandbox == "unsafe-full" else os_sandbox_backend.name
    codex_tmux = bool(
        args.codex_tmux and sandbox == "workspace-write" and not os_sandbox_backend.os_isolated
    )
    agent, allowed_agents = resolve_node_agents(args.agent, args.agents, args.cmd)
    if not allowed_agents:
        print(
            "Error: no CLI execution engine found (install claude/codex/agy/opencode or pass --cmd).",
            file=sys.stderr,
        )
        return 2
    state_dir = os.path.abspath(args.state_dir or os.path.join(os.path.expanduser("~"), ".agenct"))
    os.makedirs(state_dir, mode=0o700, exist_ok=True)
    journal = ExecutionJournal(state_dir)
    env_header = build_env_header(
        args.kind,
        agent,
        workdir,
        allowed_agents,
        sandbox=sandbox,
        isolation=node_isolation,
        allow_elevated=allow_elevated,
        codex_tmux=codex_tmux,
    )
    agents_disp = ",".join("custom" if a.startswith("custom:") else a for a in allowed_agents)
    elev_note = " elevated=1" if allow_elevated else ""
    print(
        f"[agenct-node] hub={hub} kind={args.kind} agent={agent} agents={agents_disp} "
        f"sandbox={sandbox}({node_isolation}){elev_note} "
        f"exec_backend={exec_backend_spec}({initial_backend.name}) "
        f"idle_timeout={args.idle_timeout}s workdir={workdir}",
        flush=True,
    )

    def flush_journal() -> None:
        for rec in journal.pending_submissions():
            tid = rec["task_id"]
            rev = rec["expected_revision"]
            try:
                http_json(
                    f"{hub}/node/v1/tasks/{tid}/submit",
                    token,
                    method="POST",
                    body=rec["payload"],
                    timeout=30,
                )
                journal.remove(tid, rev)
            except urllib.error.HTTPError as err:
                if err.code in (400, 404, 409):
                    journal.remove(tid, rev)
            except Exception:
                pass

    wait_secs = 0 if args.once else max(0, min(25, args.wait))
    work_url = f"{hub}/node/v1/work?wait={wait_secs}" if wait_secs > 0 else f"{hub}/node/v1/work"
    work_timeout = max(20, wait_secs + 15)

    while True:
        flush_journal()
        poll_started = time.monotonic()
        try:
            data = http_json(
                work_url,
                token,
                method="GET",
                extra_headers={"X-Agenct-Node-Env": env_header, "X-Agenct-Node-Kind": args.kind},
                timeout=work_timeout,
            )
        except Exception as exc:
            print(f"[agenct-node] poll error: {exc}", file=sys.stderr, flush=True)
            if args.once:
                return 1
            time.sleep(max(2.0, args.poll))
            continue

        job = data.get("job")
        if not job:
            if args.once:
                return 0
            elapsed = time.monotonic() - poll_started
            time.sleep(0.1 if wait_secs > 0 and elapsed >= 1.0 else args.poll)
            continue

        task_id = str(job["task_id"])
        rev = int(job["task_revision"])
        lease_token = str(job["lease_token"])
        request_id = f"node-{uuid.uuid4().hex[:16]}"
        journal.write(task_id, rev, {"state": "running", "task_id": task_id, "expected_revision": rev})

        cancel_event = threading.Event()
        stop_heartbeat = threading.Event()
        progress_dirty = threading.Event()
        prog_lock = threading.Lock()
        prog = {"stage": "Starting execution", "log_tail": ""}
        started = time.monotonic()

        def on_progress(stage: str, log_tail: str) -> None:
            with prog_lock:
                prog["stage"] = stage
                prog["log_tail"] = log_tail
            progress_dirty.set()

        def heartbeat() -> None:
            last_beat = time.monotonic()
            while not stop_heartbeat.wait(1.0):
                now = time.monotonic()
                due_keepalive = (now - last_beat) >= 15.0
                due_progress = progress_dirty.is_set() and (now - last_beat) >= 3.0
                if not due_keepalive and not due_progress:
                    continue
                progress_dirty.clear()
                with prog_lock:
                    st, tail = prog["stage"], prog["log_tail"]
                try:
                    http_json(
                        f"{hub}/node/v1/tasks/{task_id}/lease",
                        token,
                        method="POST",
                        body={
                            "lease_token": lease_token,
                            "expected_revision": rev,
                            "stage": st,
                            "log_tail": tail,
                            "elapsed_secs": int(time.monotonic() - started),
                        },
                        timeout=15,
                    )
                    last_beat = time.monotonic()
                except urllib.error.HTTPError as err:
                    if err.code in (401, 404, 409):
                        cancel_event.set()
                        return
                except Exception:
                    pass

        hb = threading.Thread(target=heartbeat, daemon=True)
        hb.start()
        chosen_agent, agent_err = select_job_agent(job.get("agent"), agent, allowed_agents)
        meta_out: dict = {}
        if chosen_agent is None:
            stop_heartbeat.set()
            hb.join(timeout=2)
            status, content, attachments = "failed", agent_err, []
            submitted_agent = str(job.get("agent") or agent).strip().lower()
        else:
            try:
                status, content, attachments = execute_job(
                    chosen_agent, job, workdir, sandbox, args.timeout, cancel_event, on_progress,
                    meta_out=meta_out, idle_timeout=args.idle_timeout, codex_tmux=codex_tmux,
                    exec_backend=exec_backend_spec, allow_elevated=allow_elevated,
                    sandbox_backend=os_sandbox_backend, state_dir=state_dir,
                )
            finally:
                stop_heartbeat.set()
                hb.join(timeout=2)
            submitted_agent = "custom" if chosen_agent.startswith("custom:") else chosen_agent

        payload = {
            "lease_token": lease_token,
            "request_id": request_id,
            "expected_revision": rev,
            # Protocol 2 accepts completed/failed. The content carries the timeout marker.
            "status": "failed" if status == "timeout" else status,
            "content": content,
            "attachments": attachments,
            "agent": submitted_agent,
        }
        if meta_out.get("agent_session_id"):
            payload["agent_session_id"] = meta_out["agent_session_id"]
        if meta_out.get("backend"):
            payload["backend"] = meta_out["backend"]
        if meta_out.get("blocked_reason"):
            payload["blocked_reason"] = meta_out["blocked_reason"]
        journal.write(
            task_id,
            rev,
            {"state": "ready_to_submit", "task_id": task_id, "expected_revision": rev, "payload": payload},
        )
        try:
            http_json(f"{hub}/node/v1/tasks/{task_id}/submit", token, method="POST", body=payload, timeout=30)
            journal.remove(task_id, rev)
            print(f"[agenct-node] finished task={task_id} status={status}", flush=True)
        except Exception as exc:
            print(f"[agenct-node] submit deferred to journal for task={task_id}: {exc}", file=sys.stderr, flush=True)

        if args.once:
            return 0


def run_self_check(args: argparse.Namespace) -> int:
    """Run minimal real headless pre-flight checks for every enabled CLI engine."""
    probe_timeout = max(15, min(args.timeout, 180))
    probe_idle_timeout = min(args.idle_timeout, probe_timeout) if args.idle_timeout > 0 else 0
    exec_backend_spec = getattr(args, "exec_backend", "auto")
    resolved_backend = resolve_exec_backend(exec_backend_spec)
    workdir = os.path.abspath(args.workdir or os.environ.get("AGENCT_WORKDIR") or os.getcwd())
    if not os.path.isdir(workdir):
        print(f"Error: workdir does not exist: {workdir}", file=sys.stderr)
        return 2
    node_sandbox = "unsafe-full" if args.unsafe_full_access else args.sandbox
    sandbox_backend_spec = getattr(args, "sandbox_backend", "auto")
    resolved_sb = resolve_sandbox_backend(sandbox_backend_spec)
    sb_disp = "direct" if node_sandbox == "unsafe-full" else resolved_sb.name
    print(
        f"[self-check] job timeout={args.timeout}s (--timeout overrides {NODE_TIMEOUT_ENV}, "
        f"default {DEFAULT_NODE_TIMEOUT}s); idle_timeout={args.idle_timeout}s; "
        f"probe timeout={probe_timeout}s; exec_backend={exec_backend_spec}({resolved_backend.name}); "
        f"sandbox_backend={sandbox_backend_spec}({sb_disp})",
        flush=True,
    )
    codex_tmux = bool(
        args.codex_tmux and node_sandbox == "workspace-write" and not resolved_sb.os_isolated
    )
    if args.codex_tmux and not codex_tmux:
        print(
            f"[self-check] Warning: --codex-tmux only takes effect under sandbox=workspace-write "
            f"(current: {node_sandbox}); ignoring.",
            file=sys.stderr,
            flush=True,
        )
    _, allowed_agents = resolve_node_agents(args.agent, args.agents, args.cmd)
    cli_engines = [a for a in allowed_agents if a in CLI_AGENTS]
    if not cli_engines:
        print("No CLI engines enabled to self-check.", file=sys.stderr)
        return 2

    all_ok = True
    probe_dir = tempfile.mkdtemp(prefix=".agenct-selfcheck-", dir=workdir)
    os.chmod(probe_dir, 0o700)
    try:
        ro_token = f"AGENCT_RO_{uuid.uuid4().hex[:8]}"
        probe_path = os.path.join(probe_dir, "probe.txt")
        with open(probe_path, "w", encoding="utf-8") as fh:
            fh.write(f"{ro_token}\n")

        for eng in cli_engines:
            t0 = time.monotonic()
            ro_status, ro_out, _ = execute_job(
                eng,
                {
                    "task_id": f"sc_ro_{eng}",
                    "title": "Self-check readonly",
                    "instruction": (
                        f"Read {probe_path} and reply with ONLY the exact token inside it. "
                        "Do not create or modify any files."
                    ),
                    "sandbox": "readonly",
                },
                workdir,
                node_sandbox,
                probe_timeout,
                threading.Event(),
                lambda _s, _t: None,
                idle_timeout=probe_idle_timeout,
                codex_tmux=codex_tmux,
                exec_backend=exec_backend_spec,
                sandbox_backend=resolved_sb,
            )
            ro_dt = time.monotonic() - t0
            dir_clean = sorted(os.listdir(probe_dir)) == ["probe.txt"]
            ro_ok = ro_status == "completed" and ro_token in ro_out and dir_clean
            ro_label = f"PASS ({ro_dt:.1f}s)" if ro_ok else f"FAIL ({ro_dt:.1f}s)"
            fail_notes: list[str] = []
            if not ro_ok:
                all_ok = False
                why = "modified probe dir" if not dir_clean else ro_out.replace("\n", " ")[:180]
                fail_notes.append(f"ro={ro_status}: {why}")

            if node_sandbox == "readonly":
                wr_label = "SKIP (node=readonly)"
            else:
                wr_token = f"AGENCT_WR_{uuid.uuid4().hex[:8]}"
                t1 = time.monotonic()
                wr_status, wr_out, _ = execute_job(
                    eng,
                    {
                        "task_id": f"sc_wr_{eng}",
                        "title": "Self-check workspace-write",
                        "instruction": (
                            f"Run a shell command to print {wr_token} (e.g. printf '%s\\n' {wr_token}) "
                            "and reply with ONLY that exact token."
                        ),
                        "sandbox": "workspace-write",
                    },
                    workdir,
                    node_sandbox,
                    probe_timeout,
                    threading.Event(),
                    lambda _s, _t: None,
                    idle_timeout=probe_idle_timeout,
                    codex_tmux=codex_tmux,
                    exec_backend=exec_backend_spec,
                    sandbox_backend=resolved_sb,
                )
                wr_dt = time.monotonic() - t1
                wr_ok = wr_status == "completed" and wr_token in wr_out
                wr_label = f"PASS ({wr_dt:.1f}s)" if wr_ok else f"FAIL ({wr_dt:.1f}s)"
                if not wr_ok:
                    all_ok = False
                    fail_notes.append(f"wr={wr_status}: {wr_out.replace(chr(10), ' ')[:180]}")

            detail = f" | {'; '.join(fail_notes)}" if fail_notes else ""
            print(f"[self-check] {eng}: readonly={ro_label} workspace-write={wr_label}{detail}", flush=True)
    finally:
        shutil.rmtree(probe_dir, ignore_errors=True)
    return 0 if all_ok else 1


def positive_timeout(value: str) -> int:
    if re.fullmatch(r"\+?[0-9]+", value.strip()):
        try:
            seconds = int(value)
        except ValueError:
            pass
        else:
            if seconds > 0:
                return seconds
    raise argparse.ArgumentTypeError("expected a positive integer number of seconds")


def non_negative_timeout(value: str) -> int:
    if re.fullmatch(r"\+?[0-9]+", value.strip()):
        try:
            seconds = int(value)
        except ValueError:
            pass
        else:
            if seconds >= 0:
                return seconds
    raise argparse.ArgumentTypeError("expected a non-negative integer number of seconds")


def _env_flag(name: str) -> bool:
    return (os.environ.get(name) or "").strip().lower() in ("1", "true", "yes", "on")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Unified Agenct Node (PC or Cloud Runner)")
    p.add_argument("--hub", default="", help="Agenct Hub origin URL (e.g. https://agenct.example.com)")
    p.add_argument("--token", default="", help="Node bearer token (or set AGENCT_NODE_TOKEN)")
    p.add_argument("--kind", choices=("pc", "cloud"), default=os.environ.get("AGENCT_NODE_KIND", "pc"))
    p.add_argument("--agent", choices=VALID_AGENTS, default=os.environ.get("AGENCT_NODE_AGENT", "auto"))
    p.add_argument(
        "--agents",
        default=os.environ.get("AGENCT_NODE_AGENTS", ""),
        help="Enabled engines ('auto' to discover all, or comma-separated like claude,codex,opencode)",
    )
    p.add_argument("--cmd", default=os.environ.get("AGENCT_NODE_CMD", ""), help="Custom execution shell command")
    p.add_argument("--workdir", default="", help="Working directory for agent execution")
    p.add_argument("--sandbox", choices=("readonly", "workspace-write"), default="workspace-write")
    p.add_argument("--unsafe-full-access", action="store_true", help="Explicitly opt into full host access")
    p.add_argument(
        "--allow-elevated",
        action="store_true",
        default=_env_flag(NODE_ALLOW_ELEVATED_ENV),
        help="Opt in to manual one-shot elevated retries (Hub retry_elevated only)",
    )
    p.add_argument(
        "--sandbox-backend",
        choices=VALID_SANDBOX_BACKENDS,
        default=None,
        help="OS sandbox backend: auto, bwrap, seatbelt, or cli-native (default: auto)",
    )
    p.add_argument(
        "--codex-tmux",
        action="store_true",
        help="Allow Codex workspace-write network access (needed for Linux tmux AF_UNIX sockets; ignored outside workspace-write)",
    )
    p.add_argument(
        "--exec-backend",
        choices=VALID_EXEC_BACKENDS,
        default=None,
        help="Execution backend: auto, tmux, conpty, wt-tail, or pipe (default: auto)",
    )
    p.add_argument("--state-dir", default="", help="Local state/journal directory")
    p.add_argument("--poll", type=float, default=3.0, help="Fallback idle poll interval in seconds")
    p.add_argument("--wait", type=int, default=25, help="Long-poll wait seconds (0 disables long-polling)")
    p.add_argument(
        "--timeout", type=positive_timeout,
        help=f"Per-job timeout in seconds (positive integer). Overrides {NODE_TIMEOUT_ENV}. "
        f"Default: {DEFAULT_NODE_TIMEOUT}; invalid environment values use the default.",
    )
    p.add_argument(
        "--idle-timeout", type=non_negative_timeout,
        help=f"Silent-hang watchdog timeout in seconds (non-negative integer; 0 disables). "
        f"Overrides {NODE_IDLE_TIMEOUT_ENV}. Default: {DEFAULT_IDLE_TIMEOUT}.",
    )
    p.add_argument("--once", action="store_true", help="Run a single poll cycle and exit")
    p.add_argument(
        "--self-check",
        action="store_true",
        help="Run real headless pre-flight checks for enabled CLI engines and exit",
    )
    parsed = p.parse_args(argv)
    if parsed.exec_backend is None:
        raw_env_backend = (os.environ.get(EXEC_BACKEND_ENV) or "").strip().lower()
        if raw_env_backend in VALID_EXEC_BACKENDS:
            parsed.exec_backend = raw_env_backend
        else:
            if raw_env_backend:
                print(f"Warning: invalid {EXEC_BACKEND_ENV}: {raw_env_backend!r}; using 'auto'.", file=sys.stderr)
            parsed.exec_backend = "auto"
    if parsed.sandbox_backend is None:
        raw_env_sb = (os.environ.get(SANDBOX_BACKEND_ENV) or "").strip().lower()
        if raw_env_sb in VALID_SANDBOX_BACKENDS:
            parsed.sandbox_backend = raw_env_sb
        else:
            if raw_env_sb:
                print(
                    f"Warning: invalid {SANDBOX_BACKEND_ENV}: {raw_env_sb!r}; using 'auto'.",
                    file=sys.stderr,
                )
            parsed.sandbox_backend = "auto"
    if parsed.timeout is None:
        parsed.timeout = DEFAULT_NODE_TIMEOUT
        if NODE_TIMEOUT_ENV in os.environ:
            try:
                parsed.timeout = positive_timeout(os.environ[NODE_TIMEOUT_ENV])
            except argparse.ArgumentTypeError as exc:
                print(f"Warning: invalid {NODE_TIMEOUT_ENV}: {exc}; using {DEFAULT_NODE_TIMEOUT}s.", file=sys.stderr)
    if parsed.idle_timeout is None:
        parsed.idle_timeout = DEFAULT_IDLE_TIMEOUT
        if NODE_IDLE_TIMEOUT_ENV in os.environ:
            try:
                parsed.idle_timeout = non_negative_timeout(os.environ[NODE_IDLE_TIMEOUT_ENV])
            except argparse.ArgumentTypeError as exc:
                print(
                    f"Warning: invalid {NODE_IDLE_TIMEOUT_ENV}: {exc}; using {DEFAULT_IDLE_TIMEOUT}s.",
                    file=sys.stderr,
                )
    if not parsed.self_check and not parsed.hub:
        p.error("the following arguments are required: --hub")
    return parsed


if __name__ == "__main__":
    configure_stdio()
    cli_args = parse_args()
    if cli_args.self_check:
        sys.exit(run_self_check(cli_args))
    sys.exit(run_node_loop(cli_args))
