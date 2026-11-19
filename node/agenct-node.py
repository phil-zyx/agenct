#!/usr/bin/env python3
"""Unified Agenct Execution Node (PC Node & Cloud Container Node).

Polls Agenct Hub using exclusive lease protocol 2, executes assigned jobs using
local coding agent CLIs (claude / codex / gemini / opencode / ollama / openai /
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
VALID_AGENTS = ("auto", "claude", "codex", "gemini", "opencode", "ollama", "openai", "template")
CLI_AGENTS = ("claude", "codex", "gemini", "opencode")
MAX_RESULT_BYTES = 16_000
MAX_EVIDENCE_BYTES = 5 * 1024 * 1024
MAX_EVIDENCE_COUNT = 4

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

    for line in raw.splitlines():
        line = line.strip()
        if not line or not line.startswith("{"):
            continue
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(ev, dict) or "type" not in ev:
            continue
        parsed_any = True
        sid = ev.get("sessionID")
        if not isinstance(sid, str):
            part_obj = ev.get("part") if isinstance(ev.get("part"), dict) else {}
            sid = part_obj.get("sessionID")
        if isinstance(sid, str) and _SAFE_SESSION_RE.match(sid.strip()):
            session_id = sid.strip()

        ev_type = ev.get("type")
        if ev_type == "text":
            part = ev.get("part") if isinstance(ev.get("part"), dict) else {}
            txt = part.get("text") if isinstance(part.get("text"), str) else ev.get("text")
            if isinstance(txt, str) and txt.strip():
                mid = str(part.get("messageID") or "")
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


def summarize_progress_line(agent: str, raw_line: str) -> str:
    """Turn a raw stdout/stderr line into clean human-readable progress text."""
    stripped = raw_line.strip()
    if agent == "opencode" and stripped.startswith("{"):
        try:
            ev = json.loads(stripped)
        except json.JSONDecodeError:
            ev = None
        if isinstance(ev, dict) and "type" in ev:
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
                return strip_ansi(str(ev.get("error") or ev.get("message") or "error")).strip()
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


def detect_agent(preferred: str = "auto", cmd: str | None = None) -> str:
    if cmd:
        return f"custom:{cmd}"
    choice = (preferred or "auto").strip().lower()
    if choice != "auto" and choice in VALID_AGENTS:
        return choice
    for tool in CLI_AGENTS:
        if shutil.which(tool):
            return tool
    if os.environ.get("AGENCT_LLM_API_KEY") or os.environ.get("OPENAI_API_KEY"):
        return "openai"
    if os.environ.get("OLLAMA_MODEL"):
        return "ollama"
    return "template"


def build_env_header(kind: str, agent: str, workdir: str) -> str:
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
    repo_part = f" | repos:{','.join(repos)}" if repos else ""
    summary = f"kind:{kind} | os:{os_name} | host:{host} | agent:{agent}{repo_part}"
    return summary[:380]


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
    input_files, in_dir = materialize_attachments(job.get("attachments") or [], workdir, sandbox)
    evidence_dir = make_scoped_tempdir("agenct-ev-", workdir, sandbox)
    try:
        prompt = build_prompt(job, input_files, evidence_dir, workdir)
        env = dict(os.environ)
        env["AGENCT_WORKDIR"] = workdir
        env["AGENCT_EVIDENCE_DIR"] = evidence_dir
        env["AGENCT_PROMPT"] = prompt
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
                if agent == "claude":
                    add_dirs = ["--add-dir", workdir, "--add-dir", evidence_dir]
                    if in_dir:
                        add_dirs.extend(["--add-dir", in_dir])
                    if sandbox == "unsafe-full":
                        add_dirs.extend(["--add-dir", os.path.expanduser("~")])
                    allowed = "Read,Glob,Grep" if sandbox == "readonly" else "Bash,Read,Glob,Grep"
                    argv = [exe, "-p", "--allowedTools", allowed, *add_dirs, "--", prompt]
                elif agent == "codex":
                    fd, out_file = tempfile.mkstemp(prefix="agenct-codex-", suffix=".txt")
                    os.close(fd)
                    sb_mode = {
                        "readonly": "read-only",
                        "workspace-write": "workspace-write",
                        "unsafe-full": "danger-full-access",
                    }.get(sandbox, "workspace-write")
                    extra = ["--add-dir", evidence_dir]
                    if in_dir:
                        extra.extend(["--add-dir", in_dir])
                    argv = [
                        exe, "exec", "--skip-git-repo-check",
                        "-s", sb_mode, "-C", workdir, *extra, "-o", out_file, prompt,
                    ]
                elif agent == "gemini":
                    argv = [exe, "-p", prompt]
                else:
                    resume_sid = str(job.get("agent_session_id") or "").strip()
                    if resume_sid and _SAFE_SESSION_RE.match(resume_sid):
                        argv = [exe, "run", "--format", "json", "--session", resume_sid, prompt]
                    else:
                        resume_sid = ""
                        argv = [exe, "run", "--format", "json", prompt]

                rc, out, err, cancelled = run_monitored(
                    argv,
                    shell=False,
                    cwd=workdir,
                    env=env,
                    timeout=timeout,
                    cancel_event=cancel_event,
                    progress_cb=progress_cb,
                    stage_label=f"Running {agent}",
                    agent_hint=agent,
                )
                if cancelled:
                    return "failed", "Job cancelled because Hub lease expired or was revoked.", []
                # If resuming an opencode session failed or produced empty output, fall back cleanly to a fresh session.
                if agent == "opencode" and resume_sid:
                    raw_probe = decode_bytes(out)
                    probe_text, _ = parse_opencode_json_result(raw_probe)
                    if rc != 0 or not (probe_text or "").strip():
                        rc, out, err, cancelled = run_monitored(
                            [exe, "run", "--format", "json", prompt],
                            shell=False,
                            cwd=workdir,
                            env=env,
                            timeout=timeout,
                            cancel_event=cancel_event,
                            progress_cb=progress_cb,
                            stage_label=f"Running {agent}",
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
                if agent == "opencode":
                    parsed, captured_sid = parse_opencode_json_result(raw_out)
                    if captured_sid and meta_out is not None:
                        meta_out["agent_session_id"] = captured_sid
                    out_text = parsed if parsed is not None else strip_ansi(raw_out).strip()
                else:
                    out_text = strip_ansi(raw_out).strip()
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
            model = os.environ.get("AGENCT_LLM_MODEL") or os.environ.get("OLLAMA_MODEL") or "gpt-4o-mini"
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
            f"- Mode: template (install claude/codex/gemini or set --cmd for execution)\n\n"
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
    agent = detect_agent(args.agent, args.cmd)
    state_dir = os.path.abspath(args.state_dir or os.path.join(os.path.expanduser("~"), ".agenct"))
    os.makedirs(state_dir, mode=0o700, exist_ok=True)
    journal = ExecutionJournal(state_dir)
    env_header = build_env_header(args.kind, agent, workdir)
    print(f"[agenct-node] hub={hub} kind={args.kind} agent={agent} sandbox={sandbox} workdir={workdir}", flush=True)

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

    while True:
        flush_journal()
        try:
            data = http_json(
                f"{hub}/node/v1/work",
                token,
                method="GET",
                extra_headers={"X-Agenct-Node-Env": env_header, "X-Agenct-Node-Kind": args.kind},
                timeout=20,
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
            time.sleep(args.poll)
            continue

        task_id = str(job["task_id"])
        rev = int(job["task_revision"])
        lease_token = str(job["lease_token"])
        request_id = f"node-{uuid.uuid4().hex[:16]}"
        journal.write(task_id, rev, {"state": "running", "task_id": task_id, "expected_revision": rev})

        cancel_event = threading.Event()
        stop_heartbeat = threading.Event()
        prog_lock = threading.Lock()
        prog = {"stage": "Starting execution", "log_tail": ""}
        started = time.monotonic()

        def on_progress(stage: str, log_tail: str) -> None:
            with prog_lock:
                prog["stage"] = stage
                prog["log_tail"] = log_tail

        def heartbeat() -> None:
            while not stop_heartbeat.wait(15.0):
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
                except urllib.error.HTTPError as err:
                    if err.code in (401, 404, 409):
                        cancel_event.set()
                        return
                except Exception:
                    pass

        hb = threading.Thread(target=heartbeat, daemon=True)
        hb.start()
        meta_out: dict = {}
        try:
            status, content, attachments = execute_job(
                agent, job, workdir, sandbox, args.timeout, cancel_event, on_progress, meta_out=meta_out
            )
        finally:
            stop_heartbeat.set()
            hb.join(timeout=2)

        payload = {
            "lease_token": lease_token,
            "request_id": request_id,
            "expected_revision": rev,
            "status": status,
            "content": content,
            "attachments": attachments,
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
    p.add_argument("--cmd", default=os.environ.get("AGENCT_NODE_CMD", ""), help="Custom execution shell command")
    p.add_argument("--workdir", default="", help="Working directory for agent execution")
    p.add_argument("--sandbox", choices=("readonly", "workspace-write"), default="workspace-write")
    p.add_argument("--unsafe-full-access", action="store_true", help="Explicitly opt into full host access")
    p.add_argument("--state-dir", default="", help="Local state/journal directory")
    p.add_argument("--poll", type=float, default=3.0, help="Idle poll interval in seconds")
    p.add_argument("--timeout", type=int, default=300, help="Per-job execution timeout in seconds")
    p.add_argument("--once", action="store_true", help="Run a single poll cycle and exit")
    return p.parse_args(argv)


if __name__ == "__main__":
    configure_stdio()
    sys.exit(run_node_loop(parse_args()))
