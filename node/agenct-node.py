#!/usr/bin/env python3
"""Unified Agenct Execution Node (PC Node & Cloud Container Node).

Polls Agenct Hub using exclusive lease protocol 2, executes assigned jobs using
local coding agent CLIs (claude / codex / agy / opencode / ollama / openai /
custom --cmd / template), renews leases with live progress, journals results for
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
import json
import locale
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid

PROTOCOL_VERSION = "2"
VALID_AGENTS = ("auto", "claude", "codex", "agy", "opencode", "ollama", "openai", "template")
CLI_AGENTS = ("claude", "codex", "agy", "opencode")
VALID_EFFORTS = ("minimal", "low", "medium", "high", "xhigh", "max")
MAX_RESULT_BYTES = 16_000
MAX_EVIDENCE_BYTES = 5 * 1024 * 1024
MAX_EVIDENCE_COUNT = 4
_SAFE_MODEL_RE = re.compile(r"^[A-Za-z0-9._:/@][A-Za-z0-9._:/@-]{0,127}$")
_SANDBOX_RANK = {"readonly": 0, "workspace-write": 1, "unsafe-full": 2}

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


def parse_opencode_json_result(raw: str) -> tuple[str | None, str | None]:
    """Extract (final_text, session_id) from `opencode run --format json` JSONL output.

    Returns:
      - (text, session_id) when JSON events were parsed (text may be "" if none emitted).
      - (None, None) when stdout is not opencode JSONL (caller should fall back to plain text).
    """
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
            err_obj = ev.get("error")
            if isinstance(err_obj, dict):
                data = err_obj.get("data") if isinstance(err_obj.get("data"), dict) else {}
                msg = data.get("message") or err_obj.get("message") or err_obj.get("name")
                if msg:
                    errors.append(str(msg))
            elif err_obj:
                errors.append(str(err_obj))
            elif ev.get("message"):
                errors.append(str(ev["message"]))

    if not parsed_any:
        return None, None
    if msg_order:
        last_mid = msg_order[-1]
        return strip_ansi("".join(msg_texts[last_mid])).strip(), session_id
    if errors:
        return strip_ansi("\n".join(errors)).strip(), session_id
    return "", session_id


def parse_opencode_json(raw: str) -> str | None:
    """Backward-compatible helper returning only the extracted final text."""
    text, _ = parse_opencode_json_result(raw)
    return text


def parse_claude_stream_json(raw: str) -> tuple[str | None, str | None]:
    """Extract (final_text, session_id) from `claude -p --output-format stream-json --verbose` JSONL."""
    parsed_any = False
    session_id: str | None = None
    result_text: str | None = None
    last_assistant_text: str | None = None
    errors: list[str] = []

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
            errs = ev.get("errors")
            if isinstance(errs, list):
                for item in errs:
                    if isinstance(item, dict) and item.get("message"):
                        errors.append(str(item["message"]))
                    elif item:
                        errors.append(str(item))
            elif ev.get("error"):
                errors.append(str(ev["error"]))
        elif ev_type == "error":
            err = ev.get("error")
            if isinstance(err, dict) and err.get("message"):
                errors.append(str(err["message"]))
            elif err or ev.get("message"):
                errors.append(str(err or ev.get("message")))

    if not parsed_any:
        return None, None
    if result_text is not None:
        return strip_ansi(result_text).strip(), session_id
    if last_assistant_text is not None:
        return strip_ansi(last_assistant_text).strip(), session_id
    if errors:
        return strip_ansi("\n".join(errors)).strip(), session_id
    return "", session_id


def parse_codex_json_result(raw: str) -> tuple[str | None, str | None]:
    """Extract (final_text, thread_id) from `codex exec --json` JSONL output."""
    parsed_any = False
    session_id: str | None = None
    last_agent_msg: str | None = None
    errors: list[str] = []

    for ev in _iter_json_events(raw):
        parsed_any = True
        session_id = _valid_session_id(ev.get("thread_id")) or _valid_session_id(ev.get("session_id")) or session_id
        ev_type = ev.get("type")
        if ev_type == "item.completed":
            item = ev.get("item") if isinstance(ev.get("item"), dict) else {}
            if item.get("type") == "agent_message" and isinstance(item.get("text"), str) and item["text"].strip():
                last_agent_msg = item["text"]
        elif ev_type in ("error", "turn.failed"):
            err = ev.get("error")
            if isinstance(err, dict) and err.get("message"):
                errors.append(str(err["message"]))
            elif err:
                errors.append(str(err))
            elif ev.get("message"):
                errors.append(str(ev["message"]))

    if not parsed_any:
        return None, None
    if last_agent_msg is not None:
        return strip_ansi(last_agent_msg).strip(), session_id
    if errors:
        return strip_ansi("\n".join(errors)).strip(), session_id
    return "", session_id


def parse_agy_stream_json(raw: str) -> tuple[str | None, str | None]:
    """Extract (final_text, conversation_id) from `agy -p ... --output-format stream-json` JSONL."""
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
            err = res.get("error")
            if isinstance(err, dict) and err.get("message"):
                errors.append(str(err["message"]))
            elif err:
                errors.append(str(err))
            denied = res.get("denied_actions")
            if isinstance(denied, list) and denied:
                labels = [
                    str(d.get("display_name") or d.get("action") or d)
                    if isinstance(d, dict)
                    else str(d)
                    for d in denied
                ]
                errors.append(f"Action denied in headless mode: {', '.join(labels)}")
        elif ev_name == "error" or ev.get("error"):
            err = ev.get("error")
            if isinstance(err, dict) and err.get("message"):
                errors.append(str(err["message"]))
            elif ev.get("message"):
                errors.append(str(ev["message"]))
            elif err:
                errors.append(str(err))

    if not parsed_any:
        return None, None
    if result_fallback is not None:
        return strip_ansi(result_fallback).strip(), session_id
    combined = strip_ansi("".join(chunks)).strip()
    if combined:
        return combined, session_id
    if errors:
        return strip_ansi("\n".join(errors)).strip(), session_id
    return "", session_id


def parse_cli_output(agent: str, raw: str) -> tuple[str | None, str | None]:
    if agent == "opencode":
        return parse_opencode_json_result(raw)
    if agent == "claude":
        return parse_claude_stream_json(raw)
    if agent == "codex":
        return parse_codex_json_result(raw)
    if agent == "agy":
        return parse_agy_stream_json(raw)
    return None, None


def summarize_progress_line(agent: str, raw_line: str) -> str:
    """Turn a raw stdout/stderr line into clean human-readable progress text."""
    stripped = raw_line.strip()
    if agent in CLI_AGENTS and stripped.startswith("{"):
        try:
            ev = json.loads(stripped)
        except json.JSONDecodeError:
            ev = None
        if isinstance(ev, dict) and ("type" in ev or "event" in ev):
            ev_type = ev.get("type")
            if agent == "opencode":
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
                    return strip_ansi(str(ev.get("error") or ev.get("message") or "error")).strip()
                return ""
            if agent == "claude":
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
                    if isinstance(ev.get("result"), str):
                        return strip_ansi(ev["result"]).strip()
                    return ""
                if ev_type == "error":
                    return strip_ansi(str(ev.get("error") or ev.get("message") or "error")).strip()
                return ""
            if agent == "codex":
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
                    err = ev.get("error")
                    msg = err.get("message") if isinstance(err, dict) else (err or ev.get("message") or "error")
                    return strip_ansi(str(msg)).strip()
                return ""
            if agent == "agy":
                ev_name = ev.get("event")
                if ev_name == "step_update":
                    su = ev.get("step_update") if isinstance(ev.get("step_update"), dict) else {}
                    stype = su.get("step_type")
                    if stype == "tool":
                        info = su.get("tool_info") if isinstance(su.get("tool_info"), dict) else {}
                        name = str(su.get("tool_name") or info.get("name") or "tool")
                        parms = info.get("parameters") if isinstance(info.get("parameters"), dict) else {}
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
                    if stype == "agent_response":
                        return strip_ansi(str(su.get("text_delta") or "")).strip()
                    return ""
                if ev_name == "result":
                    res = ev.get("result") if isinstance(ev.get("result"), dict) else {}
                    if res.get("error"):
                        return strip_ansi(str(res["error"])).strip()
                    if isinstance(res.get("response"), str) and res["response"].strip():
                        return strip_ansi(res["response"]).strip()
                    return ""
                if ev_name == "error" or ev.get("error"):
                    err = ev.get("error")
                    msg = err.get("message") if isinstance(err, dict) else (err or ev.get("message") or "error")
                    return strip_ansi(str(msg)).strip()
                return ""
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
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def discover_available_agents() -> list[str]:
    """Return all locally available engines in priority order (fallback to ['template'])."""
    found: list[str] = []
    for tool in CLI_AGENTS:
        if shutil.which(tool):
            found.append(tool)
    if os.environ.get("AGENCT_LLM_API_KEY") or os.environ.get("OPENAI_API_KEY"):
        found.append("openai")
    if os.environ.get("OLLAMA_MODEL"):
        found.append("ollama")
    return found or ["template"]


def resolve_node_agents(
    preferred: str = "auto",
    allowed_spec: str = "",
    cmd: str | None = None,
) -> tuple[str, list[str]]:
    """Resolve (default_agent, allowed_agents) for this host.

    - Custom --cmd runs exclusively as 'custom:<cmd>'.
    - Without --agents, an explicit --agent stays pinned to that single engine (safe default),
      while '--agent auto' uses '--agents' if provided or all discovered engines.
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
    else:
        default_agent = allowed[0]
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
    repo_part = f" | repos:{','.join(repos)}" if repos else ""
    summary = (
        f"kind:{kind} | os:{os_name} | host:{host} | agent:{default_label}"
        f" | agents:{','.join(agent_labels)}{repo_part}"
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
    if not os.path.isdir(evidence_dir):
        return []
    out: list[dict] = []
    for name in sorted(os.listdir(evidence_dir)):
        ext = os.path.splitext(name)[1].lower()
        mime = EVIDENCE_MIME.get(ext)
        if not mime:
            continue
        full = os.path.join(evidence_dir, name)
        if not os.path.isfile(full):
            continue
        try:
            if os.path.getsize(full) > MAX_EVIDENCE_BYTES:
                continue
            with open(full, "rb") as fh:
                raw = fh.read()
            if not raw:
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


def build_prompt(job: dict, input_files: list[str], evidence_dir: str, workdir: str) -> str:
    parts = [
        "You are an Agenct execution node handling an assigned task.",
        "Complete the request concisely using available tools and output the final Markdown result directly.",
        "Stay strictly scoped to the instruction; do not inspect unrelated directories or sensitive files (such as ~/.ssh, credentials, or secrets) unless explicitly requested.",
        f"Working directory: {workdir}",
        f"Title: {job.get('title') or ''}",
        f"Instruction:\n{job.get('instruction') or ''}",
    ]
    if job.get("context"):
        parts.append(f"Context:\n{job['context']}")
    if input_files:
        parts.append("Input attachments saved locally:\n" + "\n".join(f"- {p}" for p in input_files))
    parts.append(
        f"For text queries, output the final Markdown answer directly without creating files. "
        f"Only save screenshots or downloadable artifact files to {evidence_dir} if explicitly requested."
    )
    return "\n\n".join(parts)


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
) -> tuple[int, bytes, bytes, bool]:
    proc = subprocess.Popen(
        argv,
        shell=shell,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    out_chunks: list[bytes] = []
    err_chunks: list[bytes] = []
    tail_lock = threading.Lock()
    recent: list[str] = []

    def reader(stream, sink: list[bytes]) -> None:
        if stream is None:
            return
        try:
            while True:
                line = stream.readline()
                if not line:
                    break
                sink.append(line)
                text = summarize_progress_line(agent_hint, decode_bytes(line))
                if text:
                    with tail_lock:
                        recent.append(text[-160:])
                        if len(recent) > 4:
                            recent.pop(0)
                        tail = " | ".join(recent)[-360:]
                    progress_cb(stage_label, tail)
        except Exception:
            pass

    t_out = threading.Thread(target=reader, args=(proc.stdout, out_chunks), daemon=True)
    t_err = threading.Thread(target=reader, args=(proc.stderr, err_chunks), daemon=True)
    t_out.start()
    t_err.start()

    deadline = time.monotonic() + max(1, timeout)
    cancelled = False
    while proc.poll() is None:
        if cancel_event.is_set():
            cancelled = True
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
            break
        if time.monotonic() >= deadline:
            proc.terminate()
            try:
                proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                proc.kill()
            t_out.join(timeout=1)
            t_err.join(timeout=1)
            raise subprocess.TimeoutExpired(argv, timeout)
        time.sleep(0.2)

    t_out.join(timeout=2)
    t_err.join(timeout=2)
    return proc.returncode or 0, b"".join(out_chunks), b"".join(err_chunks), cancelled


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
) -> list[str]:
    if agent == "claude":
        add_dirs = ["--add-dir", workdir, "--add-dir", evidence_dir]
        if in_dir:
            add_dirs.extend(["--add-dir", in_dir])
        if sandbox == "unsafe-full":
            add_dirs.extend(["--add-dir", os.path.expanduser("~")])
        allowed = "Read,Glob,Grep" if sandbox == "readonly" else "Bash,Read,Glob,Grep"
        argv = [exe, "-p", "--output-format", "stream-json", "--verbose"]
        if model:
            argv.extend(["--model", model])
        if effort:
            cl_effort = "low" if effort == "minimal" else effort
            argv.extend(["--effort", cl_effort])
        if resume_sid:
            argv.extend(["--resume", resume_sid])
        argv.extend(["--allowedTools", allowed, *add_dirs, "--", prompt])
        return argv

    if agent == "codex":
        sb_mode = {
            "readonly": "read-only",
            "workspace-write": "workspace-write",
            "unsafe-full": "danger-full-access",
        }.get(sandbox, "workspace-write")
        # Codex Linux workspace-write disables network_access by default, which seccomp-blocks
        # AF_UNIX connect() (e.g. `tmux ls` on /tmp/tmux-*/default) while FS outside workdir stays ro.
        extra_cfg = ["-c", "sandbox_workspace_write.network_access=true"] if sandbox == "workspace-write" else []
        model_args = ["-m", model] if model else []
        eff_args = (
            ["-c", f'model_reasoning_effort="{"xhigh" if effort == "max" else effort}"']
            if effort
            else []
        )
        out_args = ["-o", out_file] if out_file else []
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

    if agent == "agy":
        argv = [exe, "-p", prompt, "--output-format", "stream-json"]
        if model:
            argv.extend(["--model", model])
        if effort:
            ag_effort = "low" if effort == "minimal" else effort
            argv.extend(["--effort", ag_effort])
        if sandbox == "readonly":
            argv.extend(["--mode", "plan", "--sandbox"])
        elif sandbox == "unsafe-full":
            argv.append("--dangerously-skip-permissions")
        else:
            argv.append("--sandbox")
        argv.extend(["--add-dir", workdir, "--add-dir", evidence_dir])
        if in_dir:
            argv.extend(["--add-dir", in_dir])
        if resume_sid:
            argv.extend(["--conversation", resume_sid])
        return argv

    argv = [exe, "run", "--format", "json"]
    if model:
        argv.extend(["-m", model])
    if effort:
        oc_effort = "high" if effort == "xhigh" else effort
        argv.extend(["--variant", oc_effort])
    if resume_sid:
        argv.extend(["--session", resume_sid])
    argv.append(prompt)
    return argv


def execute_job(
    agent: str,
    job: dict,
    workdir: str,
    sandbox: str,
    timeout: int,
    cancel_event: threading.Event,
    progress_cb,
    meta_out: dict | None = None,
) -> tuple[str, str, list[dict]]:
    eff_sandbox = tighten_sandbox(sandbox, job.get("sandbox"))
    job_model = sanitize_job_model(job.get("model"))
    job_effort = sanitize_job_effort(job.get("effort"))
    job_role = str(job.get("role") or "").strip()[:32]
    input_files, in_dir = materialize_attachments(job.get("attachments") or [], workdir, eff_sandbox)
    evidence_dir = make_scoped_tempdir("agenct-ev-", workdir, eff_sandbox)
    try:
        prompt = build_prompt(job, input_files, evidence_dir, workdir)
        env = dict(os.environ)
        env["AGENCT_WORKDIR"] = workdir
        env["AGENCT_EVIDENCE_DIR"] = evidence_dir
        env["AGENCT_PROMPT"] = prompt
        env["AGENCT_ROLE"] = job_role
        env["AGENCT_MODEL"] = job_model
        env["AGENCT_EFFORT"] = job_effort
        env["AGENCT_SANDBOX"] = eff_sandbox
        env["NO_COLOR"] = "1"
        env["CLICOLOR"] = "0"
        env["FORCE_COLOR"] = "0"
        env["TERM"] = "dumb"

        if agent.startswith("custom:"):
            custom_cmd = agent[len("custom:"):]
            rc, out, err, cancelled = run_monitored(
                custom_cmd,
                shell=True,
                cwd=workdir,
                env=env,
                timeout=timeout,
                cancel_event=cancel_event,
                progress_cb=progress_cb,
                stage_label="Running custom command",
                agent_hint="custom",
            )
            if cancelled:
                return "failed", "Job cancelled because Hub lease expired or was revoked.", []
            text = strip_ansi(decode_bytes(out)).strip() or strip_ansi(decode_bytes(err)).strip()
            status = "completed" if rc == 0 and text else "failed"
            return status, truncate_utf8(text or f"Command exited with code {rc}"), collect_evidence(evidence_dir)

        if agent in CLI_AGENTS:
            exe = shutil.which(agent)
            if not exe:
                return "failed", f"Agent CLI '{agent}' not found in PATH.", []
            out_file = None
            try:
                if agent == "codex":
                    fd, out_file = tempfile.mkstemp(prefix="agenct-codex-", suffix=".txt")
                    os.close(fd)
                resume_sid = _valid_session_id(job.get("agent_session_id")) or ""
                role_prefix = f"{job_role}/" if job_role else ""
                model_suffix = f" ({job_model})" if job_model else ""
                stage_label = f"Running {role_prefix}{agent}{model_suffix}"
                argv = build_cli_argv(
                    agent, exe, prompt, workdir, eff_sandbox, evidence_dir, in_dir, out_file, resume_sid,
                    model=job_model, effort=job_effort,
                )

                rc, out, err, cancelled = run_monitored(
                    argv,
                    shell=False,
                    cwd=workdir,
                    env=env,
                    timeout=timeout,
                    cancel_event=cancel_event,
                    progress_cb=progress_cb,
                    stage_label=stage_label,
                    agent_hint=agent,
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
                        if out_file and os.path.exists(out_file):
                            open(out_file, "wb").close()
                        fresh_argv = build_cli_argv(
                            agent, exe, prompt, workdir, eff_sandbox, evidence_dir, in_dir, out_file, "",
                            model=job_model, effort=job_effort,
                        )
                        rc, out, err, cancelled = run_monitored(
                            fresh_argv,
                            shell=False,
                            cwd=workdir,
                            env=env,
                            timeout=timeout,
                            cancel_event=cancel_event,
                            progress_cb=progress_cb,
                            stage_label=stage_label,
                            agent_hint=agent,
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
                status = "completed" if rc == 0 and text else "failed"
                return status, truncate_utf8(text or f"{agent} exited with code {rc}"), collect_evidence(evidence_dir)
            finally:
                if out_file and os.path.exists(out_file):
                    try:
                        os.unlink(out_file)
                    except OSError:
                        pass

        if agent in ("openai", "ollama"):
            api_url = (
                os.environ.get("AGENCT_LLM_URL")
                or ("http://127.0.0.1:11434/v1/chat/completions" if agent == "ollama" else "https://api.openai.com/v1/chat/completions")
            )
            api_key = os.environ.get("AGENCT_LLM_API_KEY") or os.environ.get("OPENAI_API_KEY") or "ollama"
            model = job_model or os.environ.get("AGENCT_LLM_MODEL") or os.environ.get("OLLAMA_MODEL") or "gpt-4o-mini"
            req_body = json.dumps({
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0.2,
            }).encode("utf-8")
            req = urllib.request.Request(
                api_url,
                data=req_body,
                headers={"Content-Type": "application/json", "Authorization": f"Bearer {api_key}"},
                method="POST",
            )
            with urllib.request.urlopen(req, timeout=min(timeout, 120)) as resp:
                data = json.loads(resp.read().decode("utf-8"))
            text = str((((data.get("choices") or [{}])[0]).get("message") or {}).get("content") or "").strip()
            return ("completed", truncate_utf8(text), []) if text else ("failed", "Empty LLM response.", [])

        # Built-in template fallback
        content = (
            f"### Node Result: {job.get('title') or job.get('task_id')}\n\n"
            f"- Workdir: `{workdir}`\n"
            f"- Mode: template (install claude/codex/agy or set --cmd for execution)\n\n"
            f"Instruction received:\n> {job.get('instruction') or ''}"
        )
        return "completed", truncate_utf8(content), []
    except subprocess.TimeoutExpired:
        return "failed", f"Execution timed out after {timeout}s.", collect_evidence(evidence_dir)
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
    agent, allowed_agents = resolve_node_agents(args.agent, args.agents, args.cmd)
    state_dir = os.path.abspath(args.state_dir or os.path.join(os.path.expanduser("~"), ".agenct"))
    os.makedirs(state_dir, mode=0o700, exist_ok=True)
    journal = ExecutionJournal(state_dir)
    env_header = build_env_header(args.kind, agent, workdir, allowed_agents)
    agents_disp = ",".join("custom" if a.startswith("custom:") else a for a in allowed_agents)
    print(
        f"[agenct-node] hub={hub} kind={args.kind} agent={agent} agents={agents_disp} sandbox={sandbox} workdir={workdir}",
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
                    chosen_agent, job, workdir, sandbox, args.timeout, cancel_event, on_progress, meta_out=meta_out
                )
            finally:
                stop_heartbeat.set()
                hb.join(timeout=2)
            submitted_agent = "custom" if chosen_agent.startswith("custom:") else chosen_agent

        payload = {
            "lease_token": lease_token,
            "request_id": request_id,
            "expected_revision": rev,
            "status": status,
            "content": content,
            "attachments": attachments,
            "agent": submitted_agent,
        }
        if meta_out.get("agent_session_id"):
            payload["agent_session_id"] = meta_out["agent_session_id"]
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


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Unified Agenct Node (PC or Cloud Runner)")
    p.add_argument("--hub", required=True, help="Agenct Hub origin URL (e.g. https://agenct.example.com)")
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
    p.add_argument("--state-dir", default="", help="Local state/journal directory")
    p.add_argument("--poll", type=float, default=3.0, help="Fallback idle poll interval in seconds")
    p.add_argument("--wait", type=int, default=25, help="Long-poll wait seconds (0 disables long-polling)")
    p.add_argument("--timeout", type=int, default=300, help="Per-job execution timeout in seconds")
    p.add_argument("--once", action="store_true", help="Run a single poll cycle and exit")
    return p.parse_args(argv)


if __name__ == "__main__":
    configure_stdio()
    sys.exit(run_node_loop(parse_args()))
