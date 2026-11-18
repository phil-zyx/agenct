#!/usr/bin/env python3
"""Local PC node daemon for Agenct.

Polls your cloud Agenct Hub for tasks explicitly assigned to this PC node,
auto-detects an installed local agent CLI (claude / codex / gemini / opencode /
openai / ollama) or uses a structured template / custom --cmd, and pushes the
resulting private draft back to the Hub for review.

Quick start (usually launched via /node/install.sh or /node/install.ps1):
  AGENCT_NODE_TOKEN=agn_... python3 agenct-node.py --hub http://10.0.0.11:5176
"""
from __future__ import annotations

import argparse
import hashlib
import threading
import base64
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
import time
import urllib.error
import urllib.request
import uuid

VALID_AGENTS = ("auto", "claude", "codex", "gemini", "opencode", "ollama", "openai", "template")
CLI_AGENTS = ("claude", "codex", "gemini", "opencode")


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


def http_json(
    url: str,
    token: str | None = None,
    method: str = "GET",
    body: dict | None = None,
    extra_headers: dict[str, str] | None = None,
    timeout: int = 20,
) -> dict:
    data = json.dumps(body, ensure_ascii=False).encode("utf-8") if body is not None else None
    headers = {"Accept": "application/json", "X-Agenct-Node-Protocol": "2"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if data is not None:
        headers["Content-Type"] = "application/json"
    if extra_headers:
        headers.update(extra_headers)
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return json.loads(resp.read().decode("utf-8"))


def truncate_utf8(text: str, max_bytes: int = 8000) -> str:
    encoded = text.encode("utf-8")
    if len(encoded) <= max_bytes:
        return text
    suffix = "\n\n...(内容超出 8000 字节上限，已截断)"
    budget = max_bytes - len(suffix.encode("utf-8"))
    return encoded[:budget].decode("utf-8", errors="ignore") + suffix


AGENT_CACHE_PATH = os.path.join(os.path.expanduser("~"), ".agenct", "agent-cache.json")


def load_cached_agent() -> str | None:
    try:
        with open(AGENT_CACHE_PATH, "r", encoding="utf-8") as fh:
            data = json.load(fh)
        agent = str((data or {}).get("agent") or "").strip().lower()
        if agent in CLI_AGENTS and shutil.which(agent):
            return agent
    except Exception:
        pass
    return None


def save_cached_agent(agent: str) -> None:
    if agent not in CLI_AGENTS:
        return
    try:
        parent = os.path.dirname(AGENT_CACHE_PATH)
        os.makedirs(parent, mode=0o700, exist_ok=True)
        tmp = f"{AGENT_CACHE_PATH}.tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump({"agent": agent}, fh)
        os.replace(tmp, AGENT_CACHE_PATH)
    except Exception:
        pass


def detect_agent(preferred: str = "auto", cmd: str | None = None) -> str:
    if cmd:
        return f"custom:{cmd}"
    choice = (preferred or "auto").strip().lower()
    if choice != "auto" and choice in VALID_AGENTS:
        return choice
    cached = load_cached_agent()
    if cached:
        return cached
    for tool in CLI_AGENTS:
        if shutil.which(tool):
            return tool
    if os.environ.get("OPENAI_API_KEY") or os.environ.get("AGENCT_LOCAL_API_KEY"):
        return "openai"
    if os.environ.get("OLLAMA_MODEL"):
        return "ollama"
    return "template"


def scan_local_repos(max_chars: int = 330) -> list[str]:
    entries: list[tuple[float, str]] = []
    seen_lower: set[str] = set()
    for root in (
        os.getcwd(),
        os.path.expanduser("~/projects/work"),
        os.path.expanduser("~/projects"),
    ):
        if not os.path.isdir(root):
            continue
        if os.path.exists(os.path.join(root, ".git")):
            name = os.path.basename(os.path.abspath(root))
            if re.fullmatch(r"[A-Za-z0-9._-]+", name) and name.lower() not in seen_lower:
                seen_lower.add(name.lower())
                try:
                    entries.append((os.path.getmtime(root), name))
                except OSError:
                    entries.append((0.0, name))
        try:
            names = os.listdir(root)
        except OSError:
            continue
        for name in names:
            if name.startswith(".") or not re.fullmatch(r"[A-Za-z0-9._-]+", name):
                continue
            if name.lower() in seen_lower:
                continue
            full = os.path.join(root, name)
            if os.path.isdir(full) and os.path.exists(os.path.join(full, ".git")):
                seen_lower.add(name.lower())
                try:
                    entries.append((os.path.getmtime(full), name))
                except OSError:
                    entries.append((0.0, name))
    entries.sort(key=lambda item: (-item[0], item[1].lower()))
    selected: list[str] = []
    used = 0
    for _, name in entries:
        extra = len(name) + (1 if selected else 0)
        if used + extra > max_chars:
            continue
        selected.append(name)
        used += extra
    return selected


def scan_active_processes(max_chars: int = 120) -> list[str]:
    system_noise = {
        "systemd", "init", "kthreadd", "sshd", "bash", "sh", "zsh", "dash", "fish",
        "ps", "grep", "egrep", "awk", "sed", "cat", "sleep", "tail", "head",
        "agenct-node", "dbus-daemon", "agetty", "login", "cron", "atd", "rsyslogd",
        "sudo", "su", "tmux", "screen",
    }
    counts: dict[str, int] = {}
    try:
        if platform.system().lower() == "windows":
            out = decode_bytes(
                subprocess.check_output(["tasklist", "/FO", "CSV", "/NH"], timeout=2, stderr=subprocess.DEVNULL)
            )
            for line in out.splitlines():
                part = line.split(",")[0].strip().strip('"').removesuffix(".exe")
                if re.fullmatch(r"[A-Za-z0-9._-]{2,24}", part) and part.lower() not in system_noise:
                    counts[part] = counts.get(part, 0) + 1
        else:
            out = decode_bytes(
                subprocess.check_output(["ps", "-eo", "comm="], timeout=2, stderr=subprocess.DEVNULL)
            )
            for line in out.splitlines():
                name = os.path.basename(line.strip())
                low = name.lower()
                if (
                    low in system_noise
                    or low.startswith(("kworker", "ksoftirq", "rcu_", "migration", "irq/", "sd-"))
                    or not re.fullmatch(r"[A-Za-z0-9._-]{2,24}", name)
                ):
                    continue
                counts[name] = counts.get(name, 0) + 1
    except Exception:
        return []

    priority = ("codex", "claude", "gemini", "opencode", "ollama", "docker", "node", "python3", "cargo", "rustc", "postgres", "redis", "nginx", "uvicorn", "java", "go")
    def rank(proc: str) -> tuple[int, int, str]:
        low = proc.lower()
        pri = 0 if any(p in low for p in priority) else 1
        return (pri, -counts[proc], low)

    selected: list[str] = []
    used = 0
    for name in sorted(counts.keys(), key=rank):
        extra = len(name) + (1 if selected else 0)
        if used + extra > max_chars:
            continue
        selected.append(name)
        used += extra
        if len(selected) >= 12:
            break
    return selected


def build_node_env(agent_mode: str) -> str:
    os_name = (platform.system() or "unknown").lower()
    arch = (platform.machine() or "unknown").lower()
    host_raw = socket.gethostname() or "pc"
    host_ascii = re.sub(r"[^A-Za-z0-9._-]", "_", host_raw).strip("_")[:40] or "pc"
    agent_label = "custom" if agent_mode.startswith("custom:") else agent_mode
    env = f"{os_name}/{arch} | host:{host_ascii} | agent:{agent_label}"
    procs = scan_active_processes(100)
    procs_segment = f" | procs:{','.join(procs)}" if procs else ""
    repo_budget = max(40, 390 - len(env) - len(procs_segment))
    repos = scan_local_repos(repo_budget)
    if repos:
        env = f"{env} | repos:{','.join(repos)}"
    if procs_segment and len(env) + len(procs_segment) <= 398:
        env = f"{env}{procs_segment}"
    ascii_only = env.encode("ascii", errors="ignore").decode("ascii")
    return ascii_only[:400]


def workspace_dirs() -> list[str]:
    candidates = [
        os.getcwd(),
        os.path.expanduser("~/projects/work"),
        os.path.expanduser("~/projects"),
        os.path.expanduser("~"),
    ]
    seen: list[str] = []
    for path in candidates:
        real = os.path.abspath(path)
        if os.path.isdir(real) and real not in seen:
            seen.append(real)
    return seen


def default_workdir() -> str:
    env_dir = (os.environ.get("AGENCT_WORKDIR") or "").strip()
    if env_dir and os.path.isdir(os.path.expanduser(env_dir)):
        return os.path.abspath(os.path.expanduser(env_dir))
    home = os.path.expanduser("~")
    for candidate in (
        os.path.expanduser("~/projects/work"),
        os.path.expanduser("~/projects"),
        os.getcwd(),
        home,
    ):
        if os.path.isdir(candidate):
            return os.path.abspath(candidate)
    return home


def resolve_job_workdir(job: dict | None = None, configured_workdir: str | None = None) -> str:
    explicit = (configured_workdir or os.environ.get("AGENCT_WORKDIR") or "").strip()
    if explicit:
        expanded = os.path.abspath(os.path.expanduser(explicit))
        if os.path.isdir(expanded):
            return expanded
    if isinstance(job, dict):
        haystack = "\n".join(
            str(job.get(k) or "") for k in ("summary", "instruction", "details")
        ).lower()
        if haystack.strip():
            best_match: tuple[int, str] | None = None
            for root in (
                os.getcwd(),
                os.path.expanduser("~/projects/work"),
                os.path.expanduser("~/projects"),
            ):
                if not os.path.isdir(root):
                    continue
                if os.path.exists(os.path.join(root, ".git")):
                    rname = os.path.basename(os.path.abspath(root))
                    if len(rname) >= 2 and rname.lower() in haystack:
                        best_match = (len(rname), os.path.abspath(root))
                try:
                    names = os.listdir(root)
                except OSError:
                    continue
                for name in names:
                    if name.startswith(".") or len(name) < 2:
                        continue
                    full = os.path.join(root, name)
                    if os.path.isdir(full) and os.path.exists(os.path.join(full, ".git")):
                        if name.lower() in haystack:
                            cand = (len(name), os.path.abspath(full))
                            if best_match is None or cand[0] > best_match[0]:
                                best_match = cand
            if best_match is not None:
                return best_match[1]
    return default_workdir()


def materialize_attachments(job: dict) -> tuple[list[str], str | None]:
    attachments = job.get("attachments") or []
    if not isinstance(attachments, list) or not attachments:
        return [], None
    ext_map = {
        "image/png": ".png",
        "image/jpeg": ".jpg",
        "image/webp": ".webp",
        "image/gif": ".gif",
        "application/pdf": ".pdf",
        "text/plain": ".txt",
        "text/markdown": ".md",
        "text/csv": ".csv",
        "application/json": ".json",
    }
    temp_dir = tempfile.mkdtemp(prefix="agenct-img-")
    try:
        os.chmod(temp_dir, 0o700)
    except OSError:
        pass
    paths: list[str] = []
    for idx, att in enumerate(attachments[:4], start=1):
        if not isinstance(att, dict):
            continue
        b64 = str(att.get("data_base64") or "").strip()
        if not b64:
            continue
        try:
            raw = base64.b64decode(b64, validate=True)
        except Exception:
            continue
        mime = str(att.get("mime_type") or "image/png")
        ext = ext_map.get(mime, ".png")
        raw_name = str(att.get("name") or f"attachment{ext}")
        safe_base = "".join(c if (c.isalnum() or c in "._-") else "_" for c in raw_name)[:48].strip("._") or "attachment"
        if "." not in safe_base or (mime.startswith("image/") and not safe_base.lower().endswith(ext)):
            safe_base = f"{safe_base}{ext}"
        target = os.path.join(temp_dir, f"{idx}_{safe_base}")
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


def collect_evidence_attachments(evidence_dir: str | None) -> list[dict]:
    if not evidence_dir or not os.path.isdir(evidence_dir):
        return []
    mime_map = {
        ".png": "image/png",
        ".jpg": "image/jpeg",
        ".jpeg": "image/jpeg",
        ".webp": "image/webp",
        ".gif": "image/gif",
        ".pdf": "application/pdf",
        ".txt": "text/plain",
        ".log": "text/plain",
        ".md": "text/markdown",
        ".markdown": "text/markdown",
        ".csv": "text/csv",
        ".json": "application/json",
    }
    items: list[dict] = []
    for name in sorted(os.listdir(evidence_dir)):
        ext = os.path.splitext(name)[1].lower()
        mime = mime_map.get(ext)
        if not mime:
            continue
        full_path = os.path.join(evidence_dir, name)
        if not os.path.isfile(full_path):
            continue
        try:
            if os.path.getsize(full_path) > 5 * 1024 * 1024:
                continue
            with open(full_path, "rb") as fh:
                raw = fh.read()
            if not raw:
                continue
            items.append({
                "name": name,
                "mime_type": mime,
                "data_base64": base64.b64encode(raw).decode("ascii"),
            })
        except OSError:
            continue
        if len(items) >= 4:
            break
    return items


def build_prompt(
    job: dict,
    image_paths: list[str] | None = None,
    evidence_dir: str | None = None,
    node: dict | None = None,
) -> str:
    dirs = workspace_dirs()
    summary = str(job.get("summary") or "").strip()
    instruction = str(job.get("instruction") or "").strip()
    is_topic_turn = bool(instruction and instruction != summary)
    node_id = str((node or {}).get("id") or "").strip()
    node_name = str((node or {}).get("name") or node_id or "").strip()
    hostname = socket.gethostname()
    parts = [
        "你正在作为用户本地 PC 上的 Agenct 执行节点处理一项协作任务。",
        "请直接使用本机工具完成检查/操作，并直接输出最终结果正文（Markdown 格式，不要输出多余寒暄或过程日志）。",
        "",
    ]
    if node_id or node_name:
        parts.append(
            f"当前执行节点标识：名称「{node_name}」、节点 ID「{node_id}」、系统主机名「{hostname}」。"
            f"若问题中提到「{node_name}」或「{node_id}」，指的就是当前这台本机，请直接采集本机数据回答，无需质疑主机名差异。"
        )
    if is_topic_turn:
        if summary:
            parts.append(f"当前归并主题：{summary}")
        parts.append(f"本次具体问题：{instruction}")
    else:
        parts.append(f"任务内容：{summary}")
    if image_paths:
        parts.extend(
            [
                "",
                "用户在本轮对话中附带了以下图片附件（已下载到本机临时路径，请直接结合图片内容回答）：",
                *[f"- {p}" for p in image_paths],
            ]
        )
    if dirs:
        parts.append("本机常用工作目录：" + ", ".join(dirs))
    if job.get("requester"):
        parts.append(f"发起人：{job['requester']}")
    if job.get("details"):
        parts.extend(["", "共享材料：", str(job["details"])])
    if job.get("base_draft"):
        label = (
            "该主题的历史上下文记录（可参考以理解追问与指代）："
            if is_topic_turn
            else "上一版草稿（可在此基础上完善）："
        )
        parts.extend(["", label, str(job["base_draft"])])
    if job.get("session_context"):
        parts.extend(
            [
                "",
                "全局会话近期上下文（包含用户在当前或其他 PC 节点上的最近对话，供你衔接上下文）：",
                str(job["session_context"]),
            ]
        )
    if evidence_dir:
        parts.extend(
            [
                "",
                "【验收证据包规范】若本次任务涉及代码修复、测试或界面/UI调整：",
                "1) 请在正文末尾附上 `### 🔍 验收证据包`（包含：改动文件、执行的验证命令与实测结果）；",
                f"2) 若涉及页面/界面视觉效果，请尽量用 Playwright/浏览器截图并将验证截图（.png/.jpg/.webp）保存至 `{evidence_dir}`，系统会自动回传给手机端看图验收。",
            ]
        )
    return "\n".join(parts)


def build_template_draft(job: dict, fallback_note: str | None = None) -> str:
    lines = [
        f"### 本地节点初稿：{job.get('summary', '')}",
        "",
        f"- 任务 ID：`{job.get('task_id')}` (v{job.get('task_revision')})",
        f"- 发起人：{job.get('requester', '')}",
    ]
    if job.get("instruction"):
        lines.append(f"- 你的补充要求：{job['instruction']}")
    if job.get("details"):
        lines.extend(["", "#### 共享材料摘要", str(job["details"])[:2000]])
    if job.get("base_draft"):
        lines.extend(["", "#### 上一版草稿参考", str(job["base_draft"])[:1500]])
    if fallback_note:
        lines.extend(["", f"> 提示：{fallback_note}"])
    else:
        lines.extend(["", "（当前节点运行在内置草稿模板模式；本机安装 Claude Code / Codex / Gemini CLI / Ollama 后重启节点可自动调用。）"])
    return truncate_utf8("\n".join(lines))


_ACTIVE_EXECUTION: dict = {"progress_cb": None, "cancel_event": None}


def _report_progress(stage: str, log_tail: str = "") -> None:
    cb = _ACTIVE_EXECUTION.get("progress_cb")
    if callable(cb):
        try:
            cb(stage, log_tail)
        except Exception:
            pass


def _terminate_proc(proc: subprocess.Popen) -> None:
    try:
        proc.terminate()
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=2)
    except Exception:
        pass


def _run_monitored_process(
    argv: list[str] | str,
    *,
    shell: bool = False,
    env: dict | None = None,
    cwd: str | None = None,
    stdin_bytes: bytes | None = None,
    timeout: int = 300,
    stage_label: str = "正在执行本机 Agent",
) -> tuple[int, bytes, bytes, bool]:
    cancel_event: threading.Event | None = _ACTIVE_EXECUTION.get("cancel_event")
    proc = subprocess.Popen(
        argv,
        shell=shell,
        env=env,
        cwd=cwd if (cwd and os.path.isdir(cwd)) else None,
        stdin=subprocess.PIPE if stdin_bytes is not None else subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    out_chunks: list[bytes] = []
    err_chunks: list[bytes] = []
    tail_lock = threading.Lock()
    recent_lines: list[str] = []

    def _reader(stream, sink: list[bytes]) -> None:
        if stream is None:
            return
        try:
            while True:
                line = stream.readline()
                if not line:
                    break
                sink.append(line)
                decoded = decode_bytes(line).strip()
                if decoded:
                    with tail_lock:
                        recent_lines.append(decoded[-160:])
                        if len(recent_lines) > 4:
                            recent_lines.pop(0)
                        tail_str = " | ".join(recent_lines)[-360:]
                    _report_progress(stage_label, tail_str)
        except Exception:
            pass

    t_out = threading.Thread(target=_reader, args=(proc.stdout, out_chunks), daemon=True)
    t_err = threading.Thread(target=_reader, args=(proc.stderr, err_chunks), daemon=True)
    t_out.start()
    t_err.start()
    if stdin_bytes is not None and proc.stdin is not None:
        try:
            proc.stdin.write(stdin_bytes)
        except Exception:
            pass
        finally:
            try:
                proc.stdin.close()
            except Exception:
                pass

    deadline = time.monotonic() + max(1, timeout)
    cancelled = False
    while proc.poll() is None:
        if cancel_event is not None and cancel_event.is_set():
            cancelled = True
            _terminate_proc(proc)
            break
        if time.monotonic() >= deadline:
            _terminate_proc(proc)
            t_out.join(timeout=1)
            t_err.join(timeout=1)
            raise subprocess.TimeoutExpired(argv, timeout)
        time.sleep(0.2)

    t_out.join(timeout=2)
    t_err.join(timeout=2)
    return proc.returncode or 0, b"".join(out_chunks), b"".join(err_chunks), cancelled


def run_cli_agent(
    tool: str,
    prompt: str,
    cmd_timeout: int,
    image_paths: list[str] | None = None,
    evidence_dir: str | None = None,
    workdir: str | None = None,
) -> tuple[bool, str]:
    exe = shutil.which(tool)
    if not exe:
        print(f"Local agent '{tool}' was not found in PATH.", file=sys.stderr, flush=True)
        return False, ""
    effective_workdir = workdir if (workdir and os.path.isdir(workdir)) else default_workdir()
    sandbox_level = (os.environ.get("AGENCT_SANDBOX") or "full").strip().lower()
    out_file: str | None = None
    if tool == "claude":
        add_dirs = ["--add-dir", effective_workdir]
        if sandbox_level == "full":
            add_dirs.extend(["--add-dir", os.path.expanduser("~")])
        if image_paths:
            add_dirs.extend(["--add-dir", os.path.dirname(image_paths[0])])
        if evidence_dir:
            add_dirs.extend(["--add-dir", evidence_dir])
        allowed_tools = "Read,Glob,Grep" if sandbox_level == "readonly" else "Bash,Read,Glob,Grep"
        argv = [exe, "-p", "--allowedTools", allowed_tools, *add_dirs, "--", prompt]
    elif tool == "codex":
        fd, out_file = tempfile.mkstemp(prefix="agenct-codex-", suffix=".txt")
        os.close(fd)
        img_args: list[str] = []
        for img_path in image_paths or []:
            if img_path.lower().endswith((".png", ".jpg", ".jpeg", ".webp", ".gif")):
                img_args.extend(["-i", img_path])
        if image_paths:
            img_args.extend(["--add-dir", os.path.dirname(image_paths[0])])
        if evidence_dir:
            img_args.extend(["--add-dir", evidence_dir])
        default_codex_sb = (
            "read-only"
            if sandbox_level == "readonly"
            else ("workspace-write" if sandbox_level == "workspace-write" else "danger-full-access")
        )
        sandbox_mode = os.environ.get("AGENCT_CODEX_SANDBOX", default_codex_sb)
        argv = [
            exe,
            "exec",
            "--skip-git-repo-check",
            "-s",
            sandbox_mode,
            "-C",
            effective_workdir,
            *img_args,
            "-o",
            out_file,
            prompt,
        ]
    elif tool == "gemini":
        argv = [exe, "-p", prompt]
    elif tool == "opencode":
        argv = [exe, "run", prompt]
    else:
        return False, ""
    env = dict(os.environ)
    env["AGENCT_WORKDIR"] = effective_workdir
    if evidence_dir:
        env["AGENCT_EVIDENCE_DIR"] = evidence_dir
    _report_progress(f"正在调用本机 {tool} 引擎（目录：{os.path.basename(effective_workdir) or effective_workdir}）…", "")
    try:
        returncode, raw_out, raw_err, cancelled = _run_monitored_process(
            argv,
            shell=False,
            env=env,
            cwd=effective_workdir,
            timeout=cmd_timeout,
            stage_label=f"本机 {tool} 正在分析与执行…",
        )
        if cancelled:
            print(f"Local agent '{tool}' was cancelled remotely by Hub.", file=sys.stderr, flush=True)
            return False, ""
        file_output = ""
        if out_file and os.path.exists(out_file):
            with open(out_file, "rb") as fh:
                file_output = decode_bytes(fh.read()).strip()
    except subprocess.TimeoutExpired:
        print(f"Local agent '{tool}' timed out after {cmd_timeout}s.", file=sys.stderr, flush=True)
        return False, ""
    except OSError as err:
        print(f"Failed to launch '{tool}': {err}", file=sys.stderr, flush=True)
        return False, ""
    finally:
        if out_file and os.path.exists(out_file):
            try:
                os.unlink(out_file)
            except OSError:
                pass
    output = file_output or decode_bytes(raw_out).strip()
    if returncode != 0 or not output:
        err_text = decode_bytes(raw_err).strip()[:300] or decode_bytes(raw_out).strip()[:300]
        print(
            f"Local agent '{tool}' failed (code={returncode}): {err_text or 'empty output'}",
            file=sys.stderr,
            flush=True,
        )
        return False, ""
    return True, output


def run_openai_agent(
    prompt: str,
    cmd_timeout: int,
    attachments: list[dict] | None = None,
) -> tuple[bool, str]:
    api_key = os.environ.get("AGENCT_LOCAL_API_KEY") or os.environ.get("OPENAI_API_KEY") or ""
    if not api_key:
        print("OPENAI_API_KEY / AGENCT_LOCAL_API_KEY is not set.", file=sys.stderr, flush=True)
        return False, ""
    base = (os.environ.get("AGENCT_LOCAL_API_BASE") or os.environ.get("OPENAI_BASE_URL") or "https://api.openai.com/v1").rstrip("/")
    model = os.environ.get("AGENCT_LOCAL_MODEL") or os.environ.get("OPENAI_MODEL") or "gpt-4o-mini"
    if attachments:
        parts: list[dict] = [{"type": "text", "text": prompt}]
        for att in attachments[:4]:
            if not isinstance(att, dict):
                continue
            b64 = str(att.get("data_base64") or "").strip()
            mime = str(att.get("mime_type") or "image/png")
            if b64:
                parts.append({"type": "image_url", "image_url": {"url": f"data:{mime};base64,{b64}"}})
        messages = [{"role": "user", "content": parts}]
    else:
        messages = [{"role": "user", "content": prompt}]
    try:
        resp = http_json(
            f"{base}/chat/completions",
            token=api_key,
            method="POST",
            body={"model": model, "messages": messages},
            timeout=cmd_timeout,
        )
        content = (((resp.get("choices") or [{}])[0].get("message") or {}).get("content") or "").strip()
        return (bool(content), content)
    except Exception as err:
        print(f"OpenAI-compatible request failed: {err}", file=sys.stderr, flush=True)
        return False, ""


def run_ollama_agent(prompt: str, cmd_timeout: int) -> tuple[bool, str]:
    host = (os.environ.get("OLLAMA_HOST") or "http://127.0.0.1:11434").rstrip("/")
    model = (os.environ.get("OLLAMA_MODEL") or "").strip()
    if not model:
        try:
            tags = http_json(f"{host}/api/tags", timeout=5)
            models = tags.get("models") or []
            if models:
                model = str(models[0].get("name") or "").strip()
        except Exception:
            model = ""
    if not model:
        print("Ollama model unavailable (set OLLAMA_MODEL or ensure Ollama is running).", file=sys.stderr, flush=True)
        return False, ""
    try:
        resp = http_json(
            f"{host}/api/chat",
            method="POST",
            body={
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
                "stream": False,
            },
            timeout=cmd_timeout,
        )
        content = ((resp.get("message") or {}).get("content") or "").strip()
        return (bool(content), content)
    except Exception as err:
        print(f"Ollama request failed: {err}", file=sys.stderr, flush=True)
        return False, ""


def run_custom_cmd(job: dict, cmd: str, cmd_timeout: int) -> tuple[str, str]:
    env = dict(os.environ)
    env.update({
        "AGENCT_TASK_ID": str(job.get("task_id") or ""),
        "AGENCT_TASK_REVISION": str(job.get("task_revision") or ""),
        "AGENCT_TASK_REQUESTER": str(job.get("requester") or ""),
        "AGENCT_TASK_SUMMARY": str(job.get("summary") or ""),
        "AGENCT_TASK_DETAILS": str(job.get("details") or ""),
        "AGENCT_TASK_INSTRUCTION": str(job.get("instruction") or ""),
        "AGENCT_BASE_DRAFT": str(job.get("base_draft") or ""),
    })
    try:
        returncode, raw_out, raw_err, cancelled = _run_monitored_process(
            cmd,
            shell=True,
            env=env,
            stdin_bytes=json.dumps(job, ensure_ascii=False).encode("utf-8"),
            timeout=cmd_timeout,
            stage_label="正在运行自定义命令…",
        )
        if cancelled:
            print("Local command was cancelled remotely by Hub.", file=sys.stderr, flush=True)
            return "failed", ""
    except subprocess.TimeoutExpired:
        print("Local command timed out.", file=sys.stderr, flush=True)
        return "failed", ""
    if returncode != 0:
        err_text = decode_bytes(raw_err).strip()[:300]
        print(f"Local command exited with code {returncode}: {err_text}", file=sys.stderr, flush=True)
        return "failed", ""
    output = decode_bytes(raw_out).strip()
    if not output:
        print("Local command produced empty stdout.", file=sys.stderr, flush=True)
        return "failed", ""
    return "completed", truncate_utf8(output)


def run_job(
    job: dict,
    agent_mode: str,
    cmd_timeout: int,
    node: dict | None = None,
) -> tuple[str, str, str, list[dict]]:
    job_workdir = resolve_job_workdir(job)
    if agent_mode.startswith("custom:"):
        status, content = run_custom_cmd(job, agent_mode[len("custom:"):], cmd_timeout)
        return status, content, agent_mode, []
    if agent_mode == "template":
        return "completed", build_template_draft(job), "template", []

    image_paths, temp_dir = materialize_attachments(job)
    evidence_dir = tempfile.mkdtemp(prefix="agenct-evidence-")
    try:
        os.chmod(evidence_dir, 0o700)
    except OSError:
        pass
    try:
        if image_paths:
            print(f"[agenct-node] 已提取 {len(image_paths)} 张图片附件至临时目录用于多模态理解。", flush=True)
        prompt = build_prompt(job, image_paths=image_paths, evidence_dir=evidence_dir, node=node)
        ok, output = False, ""
        used_agent = agent_mode
        tried: list[str] = []
        if agent_mode in CLI_AGENTS:
            candidates = [agent_mode] + [t for t in CLI_AGENTS if t != agent_mode and shutil.which(t)]
            for candidate in candidates:
                tried.append(candidate)
                ok, output = run_cli_agent(
                    candidate,
                    prompt,
                    cmd_timeout,
                    image_paths=image_paths,
                    evidence_dir=evidence_dir,
                    workdir=job_workdir,
                )
                if ok and output:
                    used_agent = candidate
                    print(f"[agenct-node] 引擎 {candidate} 执行完成。", flush=True)
                    break
                print(f"[agenct-node] {candidate} 执行未成功，尝试下一个本机引擎…", file=sys.stderr, flush=True)
        elif agent_mode == "openai":
            tried.append("openai")
            ok, output = run_openai_agent(prompt, cmd_timeout, attachments=job.get("attachments"))
        elif agent_mode == "ollama":
            tried.append("ollama")
            ok, output = run_ollama_agent(prompt, cmd_timeout)

        evidence_attachments = collect_evidence_attachments(evidence_dir)
        if evidence_attachments:
            print(f"[agenct-node] 已采集 {len(evidence_attachments)} 张验证截图作为验收证据包。", flush=True)
        if ok and output:
            if used_agent in CLI_AGENTS:
                save_cached_agent(used_agent)
            return "completed", truncate_utf8(output), used_agent, evidence_attachments
        return "completed", build_template_draft(
            job,
            fallback_note=f"本机引擎（{', '.join(tried) or agent_mode}）调用失败或暂未就绪，已自动回退为结构化草稿模板；详细原因见本机终端日志。",
        ), agent_mode, evidence_attachments
    finally:
        if temp_dir:
            shutil.rmtree(temp_dir, ignore_errors=True)
        shutil.rmtree(evidence_dir, ignore_errors=True)


def save_pending(path: str, value: dict) -> None:
    directory = os.path.dirname(path)
    fd, temporary = tempfile.mkstemp(prefix=".pending-", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(value, stream, ensure_ascii=False)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        if os.name != "nt":
            directory_fd = os.open(directory, os.O_RDONLY)
            try:
                os.fsync(directory_fd)
            finally:
                os.close(directory_fd)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def archive_pending(path: str, label: str) -> None:
    target = os.path.join(os.path.dirname(path), f"{label}-{uuid.uuid4().hex}.json")
    os.rename(path, target)
    print(f"执行记录已保留：{target}。请人工核对，不会自动重跑。", file=sys.stderr, flush=True)


def submit_pending(hub: str, token: str, path: str, pending: dict) -> bool:
    if pending.get("phase") != "result":
        archive_pending(path, "uncertain")
        return False
    try:
        http_json(f"{hub}/node/v1/tasks/{pending['task_id']}/drafts", token=token,
                  method="POST", body=pending["body"])
    except urllib.error.HTTPError as error:
        if error.code in (400, 403, 404, 409, 413, 422):
            archive_pending(path, "rejected-result")
            return False
        raise
    os.unlink(path)
    print(f"已提交任务 {pending['task_id']} 的执行结果。", flush=True)
    return True


def lock_node_state(directory: str):
    os.makedirs(directory, mode=0o700, exist_ok=True)
    if os.path.islink(directory) or not os.path.isdir(directory):
        raise ValueError("node state directory must not be a symlink")
    if os.name != "nt":
        os.chmod(directory, 0o700)
    fd = os.open(os.path.join(directory, "node.lock"), os.O_RDWR | os.O_CREAT | getattr(os, "O_NOFOLLOW", 0), 0o600)
    handle = os.fdopen(fd, "r+b")
    try:
        if os.name == "nt":
            import msvcrt
            if os.fstat(fd).st_size == 0:
                handle.write(b"0"); handle.flush()
            handle.seek(0)
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        handle.close()
        raise RuntimeError("another node process is using this state directory") from None
    return handle


def poll_once(hub: str, token: str, cmd_timeout: int, state: dict) -> bool:
    path = state["journal_path"]
    if os.path.exists(path):
        with open(path, encoding="utf-8") as stream:
            pending = json.load(stream)
        return submit_pending(hub, token, path, pending)
    payload = http_json(f"{hub}/node/v1/work", token=token,
                        extra_headers={"X-Agenct-Node-Env": state["node_env"]})
    node = payload.get("node") or {}
    if not state["connected"]:
        print(f"已连接节点 {node.get('name') or node.get('id') or 'pc'}", flush=True)
        state["connected"] = True
    job = payload.get("job")
    if not job:
        return False
    lease = job.get("lease_token")
    if not isinstance(lease, str) or len(lease) != 64:
        raise RuntimeError("Hub did not provide an exclusive execution lease; update the Hub")
    task_id, revision = job["task_id"], job["task_revision"]
    body = {"request_id": uuid.uuid4().hex, "expected_revision": revision, "lease_token": lease}
    pending = {"phase": "executing", "task_id": task_id, "body": body}
    save_pending(path, pending)  # Before any CLI or model call: a crash must never cause re-execution.
    started_at = time.monotonic()
    progress_state = {"stage": "", "log_tail": "", "dirty": False}
    progress_lock = threading.Lock()
    cancel_event = threading.Event()

    def on_progress(stage: str, log_tail: str = "") -> None:
        with progress_lock:
            if stage != progress_state["stage"] or log_tail != progress_state["log_tail"]:
                progress_state["stage"] = stage
                progress_state["log_tail"] = log_tail
                progress_state["dirty"] = True

    def renew(stage: str = "", log_tail: str = "", elapsed_secs: int = 0):
        req_body: dict = {"lease_token": lease, "expected_revision": revision}
        if stage:
            req_body["stage"] = stage
        if log_tail:
            req_body["log_tail"] = log_tail
        if elapsed_secs > 0:
            req_body["elapsed_secs"] = elapsed_secs
        return http_json(f"{hub}/node/v1/tasks/{task_id}/lease", token=token, method="POST",
                         body=req_body)

    renew()
    stopped = threading.Event()
    def heartbeat():
        ticks = 0
        while not stopped.wait(3):
            ticks += 1
            with progress_lock:
                dirty = progress_state["dirty"]
                stage = progress_state["stage"]
                log_tail = progress_state["log_tail"]
                progress_state["dirty"] = False
            if not dirty and ticks < 10:
                continue
            ticks = 0
            elapsed = max(1, int(time.monotonic() - started_at))
            try:
                renew(stage=stage, log_tail=log_tail, elapsed_secs=elapsed)
            except urllib.error.HTTPError as err:
                if err.code in (401, 403, 404, 409):
                    print(f"[agenct-node] 任务 {task_id} 租约已失效或被取消 (HTTP {err.code})，立即终止本地执行。", file=sys.stderr, flush=True)
                    cancel_event.set()
                    break
                print("租约续期失败；请检查连接。过期结果会保留供人工核对。", file=sys.stderr, flush=True)
            except Exception:
                # The Hub remains authoritative: expiration/revocation rejects the result.
                print("租约续期失败；请检查连接。过期结果会保留供人工核对。", file=sys.stderr, flush=True)
    worker = threading.Thread(target=heartbeat, daemon=True)
    worker.start()
    _ACTIVE_EXECUTION["progress_cb"] = on_progress
    _ACTIVE_EXECUTION["cancel_event"] = cancel_event
    try:
        status, content, used_agent, attachments = run_job(job, state["agent_mode"], cmd_timeout, node=node)
        if cancel_event.is_set():
            archive_pending(path, "rejected-result")
            return False
        if state.get("auto_detect") and used_agent in CLI_AGENTS:
            state["agent_mode"] = used_agent
            state["node_env"] = build_node_env(used_agent)
        body.update(status=status, content=content)
        if attachments:
            body["attachments"] = attachments
        pending["phase"] = "result"
        save_pending(path, pending)
        return submit_pending(hub, token, path, pending)
    finally:
        _ACTIVE_EXECUTION["progress_cb"] = None
        _ACTIVE_EXECUTION["cancel_event"] = None
        stopped.set()
        worker.join(timeout=21)


def maybe_reload_self(script_path: str, initial_mtime: float) -> None:
    try:
        current_mtime = os.path.getmtime(script_path)
    except OSError:
        return
    if current_mtime == initial_mtime:
        return
    time.sleep(0.2)
    try:
        with open(script_path, "r", encoding="utf-8") as fh:
            compile(fh.read(), script_path, "exec")
    except Exception:
        return
    print("[agenct-node] 检测到脚本更新，正在自动重启加载新版本…", flush=True)
    os.execv(sys.executable, [sys.executable, script_path, *sys.argv[1:]])


def main() -> int:
    configure_stdio()
    parser = argparse.ArgumentParser(description="Agenct local PC node daemon")
    parser.add_argument("--hub", default=os.environ.get("AGENCT_HUB", "http://10.0.0.11:5176"), help="Hub base URL")
    parser.add_argument("--token", default=os.environ.get("AGENCT_NODE_TOKEN", ""), help="Node token (or set AGENCT_NODE_TOKEN)")
    parser.add_argument(
        "--agent",
        default=os.environ.get("AGENCT_NODE_AGENT", "auto"),
        choices=VALID_AGENTS,
        help="Built-in agent backend (default: auto-detect)",
    )
    parser.add_argument("--cmd", default=None, help="Custom shell command to execute when a task arrives")
    parser.add_argument("--workdir", default=os.environ.get("AGENCT_WORKDIR", ""), help="Default local workspace directory for CLI agents")
    parser.add_argument(
        "--sandbox",
        default=os.environ.get("AGENCT_SANDBOX", "full"),
        choices=("readonly", "workspace-write", "full"),
        help="Execution sandbox level for local CLI agents (readonly | workspace-write | full)",
    )
    parser.add_argument("--daemon", action="store_true", help="Detach and run in the background (logs to ~/.agenct/node.log)")
    parser.add_argument("--interval", type=float, default=1.0, help="Polling interval in seconds")
    parser.add_argument("--timeout", type=int, default=300, help="Local agent timeout in seconds")
    parser.add_argument("--state-dir", help="Private local execution journal directory")
    parser.add_argument("--once", action="store_true", help="Poll once and exit")
    args = parser.parse_args()

    token = args.token.strip()
    if not token:
        print("错误: 缺少节点令牌（请设置 AGENCT_NODE_TOKEN 或传入 --token）。", file=sys.stderr)
        return 2

    os.environ["AGENCT_SANDBOX"] = args.sandbox
    if args.workdir and os.path.isdir(os.path.expanduser(args.workdir)):
        os.environ["AGENCT_WORKDIR"] = os.path.abspath(os.path.expanduser(args.workdir))

    if args.daemon and os.environ.get("_AGENCT_DAEMON_CHILD") != "1":
        agenct_dir = os.path.expanduser("~/.agenct")
        os.makedirs(agenct_dir, mode=0o700, exist_ok=True)
        log_path = os.path.join(agenct_dir, "node.log")
        pid_path = os.path.join(agenct_dir, "node.pid")
        child_env = dict(os.environ)
        child_env["_AGENCT_DAEMON_CHILD"] = "1"
        child_argv = [sys.executable, os.path.abspath(__file__), *[a for a in sys.argv[1:] if a != "--daemon"]]
        with open(log_path, "ab") as log_fh:
            proc = subprocess.Popen(
                child_argv,
                stdin=subprocess.DEVNULL,
                stdout=log_fh,
                stderr=log_fh,
                env=child_env,
                start_new_session=True,
            )
        with open(pid_path, "w", encoding="utf-8") as pid_fh:
            pid_fh.write(f"{proc.pid}\n")
        print(f"Agenct 节点已在后台常驻运行 (PID: {proc.pid})。\n  日志文件: {log_path}\n  PID 文件: {pid_path}", flush=True)
        return 0

    hub = args.hub.rstrip("/")
    agent_mode = detect_agent(args.agent, args.cmd)
    node_env = build_node_env(agent_mode)
    mode_desc = "自定义命令 (--cmd)" if agent_mode.startswith("custom:") else (
        "template (内置草稿模板；安装 Claude Code / Codex / Gemini / Ollama 后重启可自动识别)"
        if agent_mode == "template"
        else f"{agent_mode} (自动识别)"
    )
    print(f"Agenct 本地 PC 节点启动中…\n  Hub:  {hub}\n  环境: {node_env}\n  引擎: {mode_desc}", flush=True)

    script_path = os.path.abspath(__file__)
    try:
        initial_mtime = os.path.getmtime(script_path)
    except OSError:
        initial_mtime = 0.0

    identity = hashlib.sha256((hub + "\0" + token).encode()).hexdigest()[:32]
    journal_dir = os.path.abspath(args.state_dir or os.path.expanduser(f"~/.agenct/nodes/{identity}"))
    state_guard = lock_node_state(journal_dir)  # Held for the whole process lifetime.
    state = {
        "journal_path": os.path.join(journal_dir, "pending.json"),
        "connected": False,
        "agent_mode": agent_mode,
        "node_env": node_env,
        "auto_detect": args.agent == "auto" and not args.cmd,
    }
    while True:
        maybe_reload_self(script_path, initial_mtime)
        try:
            worked = poll_once(hub, token, args.timeout, state)
        except urllib.error.HTTPError as err:
            state["connected"] = False
            print(f"Hub 返回 HTTP {err.code} ({err.reason}).", file=sys.stderr, flush=True)
            if err.code == 426:
                print("节点协议已升级，请重新下载安装节点脚本。", file=sys.stderr, flush=True)
                return 1
            if err.code == 401:
                print("节点令牌无效或已重置，请回网页重新复制一键命令。", file=sys.stderr, flush=True)
                return 1
            worked = False
        except Exception as err:
            state["connected"] = False
            print(f"轮询异常 ({type(err).__name__}: {err})", file=sys.stderr, flush=True)
            worked = False
        if args.once:
            return 0
        if not worked:
            time.sleep(max(0.5, args.interval))


if __name__ == "__main__":
    raise SystemExit(main())
