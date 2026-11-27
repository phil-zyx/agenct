//! Unified Execution Node plane (PC Nodes & Cloud Container Nodes) using Lease Protocol 2.
use crate::{
    attachments,
    auth::{random_token, sha256_hex, AuthUser, User},
    automation,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, Error, Hub, Result,
};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::Notify;
use uuid::Uuid;

pub const ONLINE_WINDOW_SECS: i64 = 45;
const LEASE_TTL_SECS: i64 = 120;
const MAX_NODES_PER_USER: i64 = 16;
const MAX_WORK_WAIT_SECS: u64 = 25;

type WaiterMap = HashMap<(String, String), Weak<Notify>>;

#[derive(Clone, Default)]
pub struct NodeWaiters {
    waiters: Arc<Mutex<WaiterMap>>,
}

impl NodeWaiters {
    pub fn register(&self, user_id: &str, node_id: &str) -> Arc<Notify> {
        let mut map = self.waiters.lock().expect("node waiters mutex");
        if map.len() > 64 {
            map.retain(|_, w| w.strong_count() > 0);
        }
        let key = (user_id.to_owned(), node_id.to_owned());
        if let Some(existing) = map.get(&key).and_then(Weak::upgrade) {
            return existing;
        }
        let notify = Arc::new(Notify::new());
        map.insert(key, Arc::downgrade(&notify));
        notify
    }

    pub fn notify(&self, user_id: &str, node_id: &str) {
        let key = (user_id.to_owned(), node_id.to_owned());
        let target = {
            let mut map = self.waiters.lock().expect("node waiters mutex");
            match map.get(&key).and_then(Weak::upgrade) {
                Some(n) => Some(n),
                None => {
                    map.remove(&key);
                    None
                }
            }
        };
        if let Some(notify) = target {
            notify.notify_waiters();
        }
    }
}

#[derive(Deserialize, Default)]
pub struct WorkQuery {
    #[serde(default)]
    pub wait: u64,
}

pub const CONFIGURABLE_AGENTS: &[&str] = &[
    "claude", "codex", "agy", "opencode", "ollama", "openai", "template",
];

pub const VALID_EFFORTS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

#[derive(Clone, Debug, Serialize)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub capabilities: String,
    pub default_agent: String,
    pub allowed_agents: Vec<String>,
    pub detected_agents: Vec<String>,
    pub effective_default_agent: String,
    pub effective_agents: Vec<String>,
    pub online: bool,
    pub last_seen_at: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateNodeInput {
    #[serde(default)]
    pub id: String,
    pub name: String,
    #[serde(default = "default_kind")]
    pub kind: String,
    #[serde(default)]
    pub capabilities: String,
    #[serde(default)]
    pub default_agent: String,
    #[serde(default)]
    pub allowed_agents: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateNodeInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub default_agent: Option<String>,
    #[serde(default)]
    pub allowed_agents: Option<Vec<String>>,
}

pub fn normalize_agent_name(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "agy" | "antigravity" => Some("agy"),
        "opencode" => Some("opencode"),
        "ollama" => Some("ollama"),
        "openai" => Some("openai"),
        "template" => Some("template"),
        _ => None,
    }
}

/// Validates and normalizes an optional model identifier (safe ASCII, 1..=128 bytes, no leading `-`).
pub fn normalize_model_name(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.len() <= 128
        && !trimmed.starts_with('-')
        && trimmed.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
    {
        Ok(trimmed.to_owned())
    } else {
        Err(invalid(
            "model name must be 1..128 safe ASCII characters without spaces or leading '-'",
        ))
    }
}

/// Validates an optional reasoning effort level.
pub fn normalize_effort(raw: &str) -> Result<String> {
    let norm = raw.trim().to_ascii_lowercase();
    if norm.is_empty() {
        return Ok(String::new());
    }
    if VALID_EFFORTS.contains(&norm.as_str()) {
        Ok(norm)
    } else {
        Err(invalid("unsupported effort level"))
    }
}

/// Validates an optional job sandbox ceiling (downward-only; never accepts `unsafe-full`).
pub fn normalize_job_sandbox(raw: &str) -> Result<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(String::new()),
        "readonly" | "read-only" => Ok("readonly".into()),
        "workspace-write" => Ok("workspace-write".into()),
        _ => Err(invalid("sandbox must be 'readonly' or 'workspace-write'")),
    }
}

pub fn normalize_agents_list(list: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in list {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let norm = normalize_agent_name(trimmed)
            .ok_or_else(|| invalid("unsupported agent engine"))?
            .to_owned();
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    Ok(out)
}

fn parse_csv_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    for part in raw.split(',') {
        let item = part.trim().to_ascii_lowercase();
        if !item.is_empty()
            && item
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            && !out.contains(&item)
        {
            out.push(item);
        }
    }
    out
}

/// Extracts `(detected_default, detected_agents)` from `X-Agenct-Node-Env` capabilities string.
pub fn extract_node_detected_agents(capabilities: &str) -> (String, Vec<String>) {
    let mut default_agent = String::new();
    let mut agents = Vec::new();
    for part in capabilities.split('|') {
        let trimmed = part.trim();
        if let Some(val) = trimmed.strip_prefix("agent:") {
            let clean = val.trim().to_ascii_lowercase();
            if !clean.is_empty()
                && clean
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                default_agent = clean;
            }
        } else if let Some(val) = trimmed.strip_prefix("agents:") {
            for item in parse_csv_tokens(val) {
                if !agents.contains(&item) {
                    agents.push(item);
                }
            }
        }
    }
    if agents.is_empty() && !default_agent.is_empty() {
        agents.push(default_agent.clone());
    } else if !default_agent.is_empty() && !agents.contains(&default_agent) {
        agents.insert(0, default_agent.clone());
    }
    (default_agent, agents)
}

/// Resolves `(effective_default_agent, effective_agents, detected_agents)` for a host.
pub fn resolve_node_agents(
    default_cfg: &str,
    allowed_cfg: &str,
    capabilities: &str,
) -> (String, Vec<String>, Vec<String>) {
    let (detected_default, detected_agents) = extract_node_detected_agents(capabilities);
    let configured_allowed = parse_csv_tokens(allowed_cfg);
    let effective_agents = if !configured_allowed.is_empty() && !detected_agents.is_empty() {
        configured_allowed
            .iter()
            .filter(|a| detected_agents.contains(a))
            .cloned()
            .collect()
    } else if !detected_agents.is_empty() {
        detected_agents.clone()
    } else {
        configured_allowed.clone()
    };

    let def_cfg = default_cfg.trim().to_ascii_lowercase();
    let no_engine_metadata = configured_allowed.is_empty() && detected_agents.is_empty();
    let disjoint = !configured_allowed.is_empty()
        && !detected_agents.is_empty()
        && effective_agents.is_empty();
    let effective_default = if disjoint {
        String::new()
    } else if !def_cfg.is_empty() && (no_engine_metadata || effective_agents.contains(&def_cfg)) {
        def_cfg
    } else if !detected_default.is_empty()
        && (effective_agents.is_empty() || effective_agents.contains(&detected_default))
    {
        detected_default
    } else {
        effective_agents
            .first()
            .cloned()
            .unwrap_or(detected_default)
    };
    (effective_default, effective_agents, detected_agents)
}

/// Returns true if `node` can execute `requested_agent` (empty request matches any node).
pub fn node_supports_agent(node: &NodeView, requested_agent: &str) -> bool {
    let req = requested_agent.trim();
    if req.is_empty() {
        return true;
    }
    if node.allowed_agents.is_empty() && node.detected_agents.is_empty() {
        return true;
    }
    node.effective_agents
        .iter()
        .any(|a| a.eq_ignore_ascii_case(req))
}

fn default_kind() -> String {
    "pc".into()
}

/// Strips ANSI/VT100 terminal escape codes and stray non-printable control characters
/// while preserving UTF-8 text, newlines, and tabs.
fn sanitize_terminal_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                // CSI sequence: ESC [ <params/intermediates> <final 0x40..=0x7E>
                Some('[') => {
                    chars.next();
                    for ch in chars.by_ref() {
                        if ('@'..='~').contains(&ch) {
                            break;
                        }
                    }
                }
                // OSC sequence: ESC ] ... (BEL or ST ESC \)
                Some(']') => {
                    chars.next();
                    while let Some(ch) = chars.next() {
                        if ch == '\x07' {
                            break;
                        }
                        if ch == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // 2-byte ESC sequence
                Some(next) if ('@'..='_').contains(&next) => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        if c == '\r' {
            continue;
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

fn valid_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn slug_from_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else if matches!(c, ' ' | '-' | '_') {
                Some('-')
            } else {
                None
            }
        })
        .take(24)
        .collect();
    let trimmed = s.trim_matches('-');
    if trimmed.is_empty() {
        format!("node-{}", &Uuid::new_v4().simple().to_string()[..6])
    } else {
        trimmed.to_owned()
    }
}

#[allow(clippy::too_many_arguments)]
fn build_node_view(
    id: String,
    name: String,
    kind: String,
    capabilities: String,
    default_agent_raw: String,
    allowed_agents_raw: String,
    last_seen_at: Option<i64>,
    now: i64,
) -> NodeView {
    let allowed_agents = parse_csv_tokens(&allowed_agents_raw);
    let (effective_default_agent, effective_agents, detected_agents) =
        resolve_node_agents(&default_agent_raw, &allowed_agents_raw, &capabilities);
    let online = last_seen_at.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECS);
    NodeView {
        id,
        name,
        kind,
        capabilities,
        default_agent: default_agent_raw.trim().to_ascii_lowercase(),
        allowed_agents,
        detected_agents,
        effective_default_agent,
        effective_agents,
        online,
        last_seen_at,
    }
}

pub fn list_nodes_sync(conn: &Connection, user_id: &str) -> Result<Vec<NodeView>> {
    let now = now_unix();
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, capabilities, default_agent, allowed_agents, last_seen_at
         FROM nodes WHERE user_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(build_node_view(
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            r.get(6)?,
            now,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Extracts repository names advertised in `capabilities` (e.g. `... | repos:foo,bar`).
pub fn extract_node_repos(capabilities: &str) -> Vec<&str> {
    capabilities
        .split('|')
        .filter_map(|part| part.trim().strip_prefix("repos:"))
        .flat_map(|list| list.split(','))
        .map(str::trim)
        .filter(|repo| repo.len() >= 3)
        .collect()
}

fn instruction_mentions_repo(hint_lower: &str, repo: &str) -> bool {
    let needle = repo.to_ascii_lowercase();
    if needle.len() < 3 {
        return false;
    }
    let hay = hint_lower.as_bytes();
    let pat = needle.as_bytes();
    let mut start = 0;
    while let Some(pos) = hint_lower[start..].find(&needle) {
        let abs = start + pos;
        let end = abs + pat.len();
        let left_ok = abs == 0 || !hay[abs - 1].is_ascii_alphanumeric();
        let right_ok = end == hay.len() || !hay[end].is_ascii_alphanumeric();
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Picks the best matching node optionally constrained to `requested_agent`:
/// explicit node_id -> online repo match -> online preferred kind -> any online -> any registered.
pub fn pick_node_for_task_with_agent_sync(
    conn: &Connection,
    user_id: &str,
    preferred_id: Option<&str>,
    prefer_cloud: bool,
    instruction_hint: Option<&str>,
    requested_agent: Option<&str>,
) -> Result<Option<NodeView>> {
    let nodes = list_nodes_sync(conn, user_id)?;
    if let Some(want) = preferred_id.map(str::trim).filter(|s| !s.is_empty()) {
        if let Some(n) = nodes
            .iter()
            .find(|n| n.id.eq_ignore_ascii_case(want) || n.name.eq_ignore_ascii_case(want))
        {
            return Ok(Some(n.clone()));
        }
    }
    let req_agent = requested_agent
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    let candidates: Vec<NodeView> = nodes
        .into_iter()
        .filter(|n| node_supports_agent(n, req_agent))
        .collect();
    let want_kind = if prefer_cloud { "cloud" } else { "pc" };
    if let Some(hint) = instruction_hint.map(str::trim).filter(|s| !s.is_empty()) {
        let hint_lower = hint.to_ascii_lowercase();
        let mut best: Option<(bool, usize, usize)> = None;
        for (idx, n) in candidates.iter().enumerate() {
            if !n.online {
                continue;
            }
            let max_repo_len = extract_node_repos(&n.capabilities)
                .into_iter()
                .filter(|r| instruction_mentions_repo(&hint_lower, r))
                .map(str::len)
                .max();
            if let Some(repo_len) = max_repo_len {
                let kind_match = n.kind == want_kind;
                let replace = match best {
                    None => true,
                    Some((b_kind, b_len, _)) => {
                        (kind_match && !b_kind) || (kind_match == b_kind && repo_len > b_len)
                    }
                };
                if replace {
                    best = Some((kind_match, repo_len, idx));
                }
            }
        }
        if let Some((_, _, idx)) = best {
            return Ok(Some(candidates[idx].clone()));
        }
    }
    if let Some(n) = candidates.iter().find(|n| n.online && n.kind == want_kind) {
        return Ok(Some(n.clone()));
    }
    if let Some(n) = candidates.iter().find(|n| n.online) {
        return Ok(Some(n.clone()));
    }
    Ok(candidates.into_iter().next())
}

/// Picks the best matching node:
/// explicit node_id -> online repo match -> online preferred kind -> any online -> any registered.
pub fn pick_node_for_task_sync(
    conn: &Connection,
    user_id: &str,
    preferred_id: Option<&str>,
    prefer_cloud: bool,
    instruction_hint: Option<&str>,
) -> Result<Option<NodeView>> {
    pick_node_for_task_with_agent_sync(
        conn,
        user_id,
        preferred_id,
        prefer_cloud,
        instruction_hint,
        None,
    )
}

/// Picks the best matching node: explicit node_id -> online preferred kind -> any online -> any registered.
pub fn pick_node_sync(
    conn: &Connection,
    user_id: &str,
    preferred_id: Option<&str>,
    prefer_cloud: bool,
) -> Result<Option<NodeView>> {
    pick_node_for_task_sync(conn, user_id, preferred_id, prefer_cloud, None)
}

/// Optional metadata attached when queuing a node task.
#[derive(Default)]
pub struct EnqueueOptions<'a> {
    pub thread_id: Option<&'a str>,
    pub on_complete: &'a str,
    pub callback_depth: i64,
    pub agent: &'a str,
    pub role: &'a str,
    pub model: &'a str,
    pub effort: &'a str,
    pub sandbox: &'a str,
}

/// Creates a task and queues a node job atomically in SQLite.
pub fn create_task_and_enqueue_sync(
    conn: &mut Connection,
    requester_id: &str,
    owner_id: &str,
    node_id: &str,
    title: &str,
    instruction: &str,
    authorization: &str,
) -> Result<(String, i64)> {
    create_task_and_enqueue_in_thread_sync(
        conn,
        requester_id,
        owner_id,
        node_id,
        title,
        instruction,
        authorization,
        None,
    )
}

/// Creates a task (optionally bound to an implicit backend thread) and queues a node job atomically.
#[allow(clippy::too_many_arguments)]
pub fn create_task_and_enqueue_in_thread_sync(
    conn: &mut Connection,
    requester_id: &str,
    owner_id: &str,
    node_id: &str,
    title: &str,
    instruction: &str,
    authorization: &str,
    thread_id: Option<&str>,
) -> Result<(String, i64)> {
    create_task_and_enqueue_with_options_sync(
        conn,
        requester_id,
        owner_id,
        node_id,
        title,
        instruction,
        authorization,
        EnqueueOptions {
            thread_id,
            ..Default::default()
        },
    )
}

/// Creates a task with explicit thread & callback options and queues a node job atomically.
#[allow(clippy::too_many_arguments)]
pub fn create_task_and_enqueue_with_options_sync(
    conn: &mut Connection,
    requester_id: &str,
    owner_id: &str,
    node_id: &str,
    title: &str,
    instruction: &str,
    authorization: &str,
    opts: EnqueueOptions<'_>,
) -> Result<(String, i64)> {
    let now = now_unix();
    let task_id = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
    let clean_title = truncate_bytes(title.trim(), 160);
    let clean_instr = truncate_bytes(instruction.trim(), 8000);
    let clean_cb = truncate_bytes(opts.on_complete.trim(), 1000);
    let clean_agent = opts.agent.trim();
    let clean_role = truncate_bytes(opts.role.trim(), 64);
    let clean_model = truncate_bytes(opts.model.trim(), 128);
    let clean_effort = truncate_bytes(opts.effort.trim(), 16);
    let clean_sandbox = truncate_bytes(opts.sandbox.trim(), 24);
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO tasks(id, requester_id, owner_id, thread_id, title, instruction, status, revision, on_complete, callback_depth, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running', 1, ?7, ?8, ?9, ?9)",
        params![
            task_id,
            requester_id,
            owner_id,
            opts.thread_id,
            clean_title,
            clean_instr,
            clean_cb,
            opts.callback_depth,
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO node_jobs(task_id, user_id, node_id, agent, role, model, effort, sandbox, task_revision, authorization, status, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, 'queued', ?10)",
        params![
            task_id,
            owner_id,
            node_id,
            clean_agent,
            clean_role,
            clean_model,
            clean_effort,
            clean_sandbox,
            authorization,
            now
        ],
    )?;
    tx.commit()?;
    Ok((task_id, 1))
}

pub async fn list(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Vec<NodeView>>> {
    let uid = user.id;
    let nodes = hub.db.read(move |conn| list_nodes_sync(conn, &uid)).await?;
    Ok(Json(nodes))
}

pub async fn create(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<CreateNodeInput>,
) -> Result<Json<Value>> {
    let name = input.name.trim();
    if name.is_empty() || name.len() > 80 {
        return Err(invalid("node name must be 1..80 bytes"));
    }
    if !matches!(input.kind.as_str(), "pc" | "cloud") {
        return Err(invalid("node kind must be 'pc' or 'cloud'"));
    }
    let req_id = if input.id.trim().is_empty() {
        slug_from_name(name)
    } else {
        let id = input.id.trim().to_owned();
        if !valid_node_id(&id) {
            return Err(invalid("invalid node id"));
        }
        id
    };
    let default_agent = if input.default_agent.trim().is_empty() {
        String::new()
    } else {
        normalize_agent_name(&input.default_agent)
            .ok_or_else(|| invalid("unsupported default_agent"))?
            .to_owned()
    };
    let mut allowed_agents = normalize_agents_list(&input.allowed_agents)?;
    if !default_agent.is_empty()
        && !allowed_agents.is_empty()
        && !allowed_agents.contains(&default_agent)
    {
        allowed_agents.insert(0, default_agent.clone());
    }
    let allowed_csv = allowed_agents.join(",");

    let token = format!("agn_{}", random_token());
    let token_hash = sha256_hex(&token);
    let uid = user.id.clone();
    let name_owned = name.to_owned();
    let kind = input.kind;
    let caps = truncate_bytes(input.capabilities.trim(), 560);
    let id_clone = req_id.clone();
    let def_db = default_agent.clone();
    let allow_db = allowed_csv.clone();

    let created = hub
        .db
        .write(move |conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM nodes WHERE user_id = ?1",
                [&uid],
                |r| r.get(0),
            )?;
            if count >= MAX_NODES_PER_USER {
                return Err(invalid("maximum number of nodes reached (16)"));
            }
            let exists: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM nodes WHERE user_id = ?1 AND id = ?2)",
                params![uid, id_clone],
                |r| r.get(0),
            )?;
            let final_id = if exists {
                format!("{id_clone}-{}", &Uuid::new_v4().simple().to_string()[..4])
            } else {
                id_clone
            };
            let now = now_unix();
            conn.execute(
                "INSERT INTO nodes(id, user_id, name, kind, capabilities, default_agent, allowed_agents, token_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![final_id, uid, name_owned, kind, caps, def_db, allow_db, token_hash, now],
            )?;
            record_audit(
                conn,
                &uid,
                "node_create",
                "ok",
                &format!("Added {kind} node {name_owned}"),
                &format!("id={final_id}"),
            )?;
            Ok(build_node_view(
                final_id,
                name_owned,
                kind,
                caps,
                def_db,
                allow_db,
                None,
                now,
            ))
        })
        .await?;

    hub.events.emit(
        user.id,
        "node",
        json!({"action": "created", "node": created}),
    );

    let mut flags = Vec::new();
    if created.kind == "cloud" {
        flags.push("--kind cloud".to_owned());
    }
    if !created.default_agent.is_empty() {
        flags.push(format!("--agent {}", created.default_agent));
    }
    if !created.allowed_agents.is_empty() {
        flags.push(format!("--agents {}", created.allowed_agents.join(",")));
    } else if created.default_agent.is_empty() {
        flags.push("--agents auto".to_owned());
    }
    let flags_suffix = if flags.is_empty() {
        String::new()
    } else {
        format!(" {}", flags.join(" "))
    };

    let origin = &hub.config.public_origin;
    Ok(Json(json!({
        "node": created,
        "token": token,
        "install_unix": format!("curl -fsSL {origin}/node/install.sh | sh -s -- {token}{flags_suffix}"),
        "install_ps": format!("irm {origin}/node/install.ps1 | iex; agenct-node --hub {origin} --token {token}{flags_suffix}"),
    })))
}

pub(crate) async fn update_node_config(
    hub: &Hub,
    user_id: &str,
    node_id: &str,
    name: Option<&str>,
    default_agent: Option<&str>,
    allowed_agents: Option<&[String]>,
) -> Result<NodeView> {
    let next_name = match name {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() || trimmed.len() > 80 {
                return Err(invalid("node name must be 1..80 bytes"));
            }
            Some(trimmed.to_owned())
        }
        None => None,
    };
    let next_default = match default_agent {
        Some(raw) if raw.trim().is_empty() => Some(String::new()),
        Some(raw) => Some(
            normalize_agent_name(raw)
                .ok_or_else(|| invalid("unsupported default_agent"))?
                .to_owned(),
        ),
        None => None,
    };
    let next_allowed = match allowed_agents {
        Some(list) => Some(normalize_agents_list(list)?),
        None => None,
    };

    let uid = user_id.to_owned();
    let nid = node_id.to_owned();
    let updated = hub
        .db
        .write(move |conn| {
            struct RawNode {
                name: String,
                kind: String,
                capabilities: String,
                default_agent: String,
                allowed_agents: String,
                last_seen_at: Option<i64>,
            }
            let cur: Option<RawNode> = conn
                .query_row(
                    "SELECT name, kind, capabilities, default_agent, allowed_agents, last_seen_at
                     FROM nodes WHERE user_id = ?1 AND id = ?2",
                    params![uid, nid],
                    |r| {
                        Ok(RawNode {
                            name: r.get(0)?,
                            kind: r.get(1)?,
                            capabilities: r.get(2)?,
                            default_agent: r.get(3)?,
                            allowed_agents: r.get(4)?,
                            last_seen_at: r.get(5)?,
                        })
                    },
                )
                .optional()?;
            let Some(cur) = cur else {
                return Err(denied());
            };
            let name = next_name.unwrap_or(cur.name);
            let default_agent = next_default.unwrap_or(cur.default_agent);
            let mut allowed_list =
                next_allowed.unwrap_or_else(|| parse_csv_tokens(&cur.allowed_agents));
            if !default_agent.is_empty()
                && !allowed_list.is_empty()
                && !allowed_list.contains(&default_agent)
            {
                allowed_list.insert(0, default_agent.clone());
            }
            let allowed_csv = allowed_list.join(",");
            conn.execute(
                "UPDATE nodes SET name = ?3, default_agent = ?4, allowed_agents = ?5
                 WHERE user_id = ?1 AND id = ?2",
                params![uid, nid, name, default_agent, allowed_csv],
            )?;
            record_audit(
                conn,
                &uid,
                "node_update",
                "ok",
                &format!("Updated node {name}"),
                &format!("id={nid} default={default_agent} allowed={allowed_csv}"),
            )?;
            Ok(build_node_view(
                nid,
                name,
                cur.kind,
                cur.capabilities,
                default_agent,
                allowed_csv,
                cur.last_seen_at,
                now_unix(),
            ))
        })
        .await?;

    hub.events.emit(
        user_id.to_owned(),
        "node",
        json!({"action": "updated", "node": updated}),
    );
    Ok(updated)
}

pub async fn update(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(node_id): Path<String>,
    Json(input): Json<UpdateNodeInput>,
) -> Result<Json<NodeView>> {
    update_node_config(
        &hub,
        &user.id,
        &node_id,
        input.name.as_deref(),
        input.default_agent.as_deref(),
        input.allowed_agents.as_deref(),
    )
    .await
    .map(Json)
}

pub(crate) fn fail_orphaned_running_tasks_sync(
    conn: &Connection,
    user_id: Option<&str>,
    reason: &str,
    now: i64,
) -> Result<Vec<(String, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT owner_id, requester_id, id
         FROM tasks
         WHERE status = 'running'
           AND (?1 IS NULL OR owner_id = ?1)
           AND NOT EXISTS (
               SELECT 1 FROM node_jobs j
               WHERE j.task_id = tasks.id AND j.status IN ('queued', 'running')
           )",
    )?;
    let rows = stmt
        .query_map(params![user_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !rows.is_empty() {
        conn.execute(
            "UPDATE tasks
             SET status = 'failed',
                 result = COALESCE(NULLIF(result, ''), ?2),
                 revision = revision + 1,
                 updated_at = ?3
             WHERE status = 'running'
               AND (?1 IS NULL OR owner_id = ?1)
               AND NOT EXISTS (
                   SELECT 1 FROM node_jobs j
                   WHERE j.task_id = tasks.id AND j.status IN ('queued', 'running')
               )",
            params![user_id, reason, now],
        )?;
    }
    Ok(rows)
}

pub(crate) fn emit_failed_tasks(hub: &Hub, failed: Vec<(String, String, String)>) {
    for (owner_id, requester_id, task_id) in failed {
        let payload = json!({"task_id": task_id, "status": "failed"});
        if requester_id != owner_id {
            hub.events.emit(requester_id, "task", payload.clone());
        }
        hub.events.emit(owner_id, "task", payload);
    }
}

pub(crate) fn reap_expired_leases_sync(
    conn: &Connection,
    scope: Option<(&str, Option<&str>)>,
    now: i64,
) -> Result<Vec<(String, String, String)>> {
    match scope {
        Some((uid, Some(nid))) => {
            conn.execute(
                "UPDATE node_jobs
                 SET status='failed', lease_hash=NULL, lease_until=NULL, stage='lease_expired', updated_at=?3
                 WHERE user_id=?1 AND node_id=?2 AND status='running'
                   AND (lease_until IS NULL OR lease_until <= ?3)",
                params![uid, nid, now],
            )?;
        }
        Some((uid, None)) => {
            conn.execute(
                "UPDATE node_jobs
                 SET status='failed', lease_hash=NULL, lease_until=NULL, stage='lease_expired', updated_at=?2
                 WHERE user_id=?1 AND status='running'
                   AND (lease_until IS NULL OR lease_until <= ?2)",
                params![uid, now],
            )?;
        }
        None => {
            conn.execute(
                "UPDATE node_jobs
                 SET status='failed', lease_hash=NULL, lease_until=NULL, stage='lease_expired', updated_at=?1
                 WHERE status='running'
                   AND (lease_until IS NULL OR lease_until <= ?1)",
                params![now],
            )?;
        }
    }
    fail_orphaned_running_tasks_sync(
        conn,
        scope.map(|(uid, _)| uid),
        "节点执行中断或租约已过期",
        now,
    )
}

pub async fn reap_expired_leases(hub: &Hub) {
    if let Ok(failed) = hub
        .db
        .write(|conn| {
            let tx = conn.transaction()?;
            let failed = reap_expired_leases_sync(&tx, None, now_unix())?;
            tx.commit()?;
            Ok(failed)
        })
        .await
    {
        emit_failed_tasks(hub, failed);
    }
}

pub async fn rotate_token(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(node_id): Path<String>,
) -> Result<Json<Value>> {
    let token = format!("agn_{}", random_token());
    let token_hash = sha256_hex(&token);
    let uid = user.id.clone();
    let nid = node_id.clone();

    let failed = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let changed = tx.execute(
                "UPDATE nodes SET token_hash = ?3 WHERE user_id = ?1 AND id = ?2",
                params![uid, nid, token_hash],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            let now = now_unix();
            // Invalidate any running leases held by the rotated token.
            tx.execute(
                "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?3
                 WHERE user_id = ?1 AND node_id = ?2 AND status = 'running'",
                params![uid, nid, now],
            )?;
            let failed = fail_orphaned_running_tasks_sync(
                &tx,
                Some(&uid),
                "节点令牌已重置，原任务已中止",
                now,
            )?;
            tx.commit()?;
            Ok(failed)
        })
        .await?;

    emit_failed_tasks(&hub, failed);
    hub.node_waiters.notify(&user.id, &node_id);
    Ok(Json(json!({"id": node_id, "token": token})))
}

pub async fn remove(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(node_id): Path<String>,
) -> Result<Json<Value>> {
    let uid = user.id.clone();
    let nid = node_id.clone();
    let failed = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let changed = tx.execute(
                "DELETE FROM nodes WHERE user_id = ?1 AND id = ?2",
                params![uid, nid],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            let failed = fail_orphaned_running_tasks_sync(
                &tx,
                Some(&uid),
                "执行节点已移除，任务已中止",
                now_unix(),
            )?;
            tx.commit()?;
            Ok(failed)
        })
        .await?;
    emit_failed_tasks(&hub, failed);
    hub.node_waiters.notify(&user.id, &node_id);
    hub.events
        .emit(user.id, "node", json!({"action": "removed", "id": node_id}));
    Ok(Json(json!({"ok": true, "id": node_id})))
}

async fn authenticate_node(hub: &Hub, headers: &HeaderMap) -> Result<(String, String, String)> {
    if headers
        .get("x-agenct-node-protocol")
        .and_then(|v| v.to_str().ok())
        != Some("2")
    {
        return Err(Error(
            StatusCode::UPGRADE_REQUIRED,
            "node protocol 2 required",
        ));
    }
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(Error(
            StatusCode::UNAUTHORIZED,
            "node bearer token required",
        ))?;
    if !token.starts_with("agn_")
        || token.len() != 68
        || !token[4..].bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(Error(StatusCode::UNAUTHORIZED, "invalid node token"));
    }
    let digest = sha256_hex(token);
    let env_info = headers
        .get("x-agenct-node-env")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 600 && !s.chars().any(char::is_control))
        .map(str::to_owned);
    let kind_hdr = headers
        .get("x-agenct-node-kind")
        .and_then(|v| v.to_str().ok())
        .filter(|k| matches!(*k, "pc" | "cloud"))
        .map(str::to_owned);

    hub.db
        .write(move |conn| {
            let row: Option<(String, String, String, String)> = conn
                .query_row(
                    "SELECT n.user_id, n.id, n.name, u.status
                     FROM nodes n JOIN users u ON u.id = n.user_id
                     WHERE n.token_hash = ?1",
                    [&digest],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let (user_id, node_id, node_name, status) =
                row.ok_or(Error(StatusCode::UNAUTHORIZED, "invalid node token"))?;
            if status != "active" {
                return Err(Error(StatusCode::UNAUTHORIZED, "account disabled"));
            }
            let now = now_unix();
            conn.execute(
                "UPDATE nodes
                 SET last_seen_at = ?3,
                     capabilities = COALESCE(?4, capabilities),
                     kind = COALESCE(?5, kind)
                 WHERE user_id = ?1 AND id = ?2",
                params![user_id, node_id, now, env_info, kind_hdr],
            )?;
            Ok((user_id, node_id, node_name))
        })
        .await
}

struct ClaimedWork {
    task_id: String,
    task_revision: i64,
    lease_token: String,
    lease_until: i64,
    title: String,
    instruction: String,
    agent: Option<String>,
    role: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    sandbox: Option<String>,
    authorization: String,
    thread_id: Option<String>,
    agent_session_id: Option<String>,
}

async fn try_claim_work(hub: &Hub, user_id: &str, node_id: &str) -> Result<Option<ClaimedWork>> {
    let uid = user_id.to_owned();
    let nid = node_id.to_owned();
    let (claimed, failed_tasks) = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let node_cfg: Option<(String, String)> = tx
                .query_row(
                    "SELECT default_agent, capabilities FROM nodes WHERE user_id = ?1 AND id = ?2",
                    params![uid, nid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((node_default_agent, node_caps)) = node_cfg else {
                return Err(Error(StatusCode::UNAUTHORIZED, "invalid node token"));
            };
            let perms = automation::read_permissions_sync(&tx, &uid)?;
            let mut failed_tasks = automation::revoke_disallowed_jobs_sync(&tx, &uid, &perms)?;
            let now = now_unix();
            failed_tasks.extend(reap_expired_leases_sync(&tx, Some((&uid, Some(&nid))), now)?);
            struct PendingJob {
                task_id: String,
                task_revision: i64,
                title: String,
                instruction: String,
                thread_id: Option<String>,
                agent: String,
                role: String,
                model: String,
                effort: String,
                sandbox: String,
                authorization: String,
            }
            let pending: Option<PendingJob> = tx
                .query_row(
                    "SELECT j.task_id, j.task_revision, t.title, t.instruction, t.thread_id,
                            j.agent, j.role, j.model, j.effort, j.sandbox, j.authorization
                     FROM node_jobs j
                     JOIN tasks t ON t.id = j.task_id
                     WHERE j.user_id = ?1 AND j.node_id = ?2
                       AND j.status = 'queued' AND t.status = 'running' AND t.revision = j.task_revision
                     ORDER BY j.updated_at ASC LIMIT 1",
                    params![uid, nid],
                    |r| {
                        Ok(PendingJob {
                            task_id: r.get(0)?,
                            task_revision: r.get(1)?,
                            title: r.get(2)?,
                            instruction: r.get(3)?,
                            thread_id: r.get(4)?,
                            agent: r.get(5)?,
                            role: r.get(6)?,
                            model: r.get(7)?,
                            effort: r.get(8)?,
                            sandbox: r.get(9)?,
                            authorization: r.get(10)?,
                        })
                    },
                )
                .optional()?;
            let Some(PendingJob {
                task_id,
                task_revision,
                title,
                instruction,
                thread_id,
                agent: job_agent,
                role: job_role,
                model: job_model,
                effort: job_effort,
                sandbox: job_sandbox,
                authorization: job_authorization,
            }) = pending
            else {
                tx.commit()?;
                return Ok((None, failed_tasks));
            };
            let (detected_default, _) = extract_node_detected_agents(&node_caps);
            let requested_or_default = if !job_agent.trim().is_empty() {
                job_agent.trim().to_ascii_lowercase()
            } else {
                node_default_agent.trim().to_ascii_lowercase()
            };
            let effective_job_agent = if !requested_or_default.is_empty() {
                requested_or_default.clone()
            } else {
                detected_default.clone()
            };
            let agent_session_id: Option<String> = if let Some(ref thr_id) = thread_id {
                let sess: Option<(String, String, String, String)> = tx
                    .query_row(
                        "SELECT node_session_id, node_session_agent, node_session_role, node_session_model
                         FROM threads
                         WHERE id = ?1 AND user_id = ?2 AND node_id = ?3
                           AND node_session_id IS NOT NULL AND node_session_id != ''",
                        params![thr_id, uid, nid],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                sess.and_then(|(sid, s_agent, s_role, s_model)| {
                    let s_agent_trim = s_agent.trim();
                    let agent_matched = if s_agent_trim.is_empty() {
                        requested_or_default.is_empty()
                            || (!detected_default.is_empty()
                                && requested_or_default.eq_ignore_ascii_case(&detected_default))
                    } else {
                        !effective_job_agent.is_empty()
                            && s_agent_trim.eq_ignore_ascii_case(&effective_job_agent)
                    };
                    let role_matched = s_role.trim().eq_ignore_ascii_case(job_role.trim());
                    let model_matched = s_model.trim() == job_model.trim();
                    (agent_matched && role_matched && model_matched).then_some(sid)
                })
            } else {
                None
            };
            let lease_token = random_token();
            let lease_until = now + LEASE_TTL_SECS;
            tx.execute(
                "UPDATE node_jobs
                 SET status='running', lease_hash=?2, lease_until=?3, submission_hash=NULL,
                     stage='Claimed by node', updated_at=?4
                 WHERE task_id = ?1",
                params![task_id, sha256_hex(&lease_token), lease_until, now],
            )?;
            tx.commit()?;
            let opt_non_empty = |s: String| {
                let t = s.trim();
                (!t.is_empty()).then(|| t.to_owned())
            };
            Ok((
                Some(ClaimedWork {
                    task_id,
                    task_revision,
                    lease_token,
                    lease_until,
                    title,
                    instruction,
                    agent: (!requested_or_default.is_empty()).then_some(requested_or_default),
                    role: opt_non_empty(job_role),
                    model: opt_non_empty(job_model),
                    effort: opt_non_empty(job_effort),
                    sandbox: opt_non_empty(job_sandbox),
                    authorization: job_authorization,
                    thread_id,
                    agent_session_id,
                }),
                failed_tasks,
            ))
        })
        .await?;
    emit_failed_tasks(hub, failed_tasks);
    Ok(claimed)
}

pub async fn work(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Query(q): Query<WorkQuery>,
) -> Result<Json<Value>> {
    let (user_id, node_id, node_name) = authenticate_node(&hub, &headers).await?;
    let wait_secs = q.wait.min(MAX_WORK_WAIT_SECS);

    let claimed = if wait_secs == 0 {
        try_claim_work(&hub, &user_id, &node_id).await?
    } else {
        let waiter = hub.node_waiters.register(&user_id, &node_id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(wait_secs);
        loop {
            let notified = waiter.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if let Some(work) = try_claim_work(&hub, &user_id, &node_id).await? {
                break Some(work);
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break None;
            }
            tokio::select! {
                _ = &mut notified => {}
                _ = tokio::time::sleep(remaining) => break None,
            }
        }
    };

    let Some(ClaimedWork {
        task_id,
        task_revision,
        lease_token,
        lease_until,
        title,
        instruction,
        agent,
        role,
        model,
        effort,
        sandbox,
        authorization,
        thread_id,
        agent_session_id,
    }) = claimed
    else {
        return Ok(Json(json!({
            "node": {"id": node_id, "name": node_name},
            "job": Value::Null,
        })));
    };

    let atts = attachments::collect_for_user(&hub, &user_id, &instruction).await?;
    hub.events.emit(
        user_id,
        "task",
        json!({
            "task_id": task_id,
            "status": "running",
            "stage": "Claimed by node",
            "node_id": node_id,
            "agent": agent,
            "role": role,
            "model": model,
        }),
    );

    Ok(Json(json!({
        "node": {"id": node_id, "name": node_name},
        "job": {
            "task_id": task_id,
            "task_revision": task_revision,
            "lease_token": lease_token,
            "lease_until": lease_until,
            "title": title,
            "instruction": instruction,
            "agent": agent,
            "role": role,
            "model": model,
            "effort": effort,
            "sandbox": sandbox,
            "authorization": authorization,
            "thread_id": thread_id,
            "agent_session_id": agent_session_id,
            "attachments": atts,
        }
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewLeaseInput {
    lease_token: String,
    expected_revision: i64,
    #[serde(default)]
    stage: String,
    #[serde(default)]
    log_tail: String,
    #[serde(default)]
    elapsed_secs: u64,
}

pub async fn renew_lease(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<RenewLeaseInput>,
) -> Result<Json<Value>> {
    let (user_id, node_id, _) = authenticate_node(&hub, &headers).await?;
    let clean_stage = truncate_bytes(sanitize_terminal_text(&input.stage).trim(), 120);
    let clean_tail = truncate_bytes(sanitize_terminal_text(&input.log_tail).trim(), 360);
    let lease_hash = sha256_hex(&input.lease_token);
    let uid = user_id.clone();
    let tid = task_id.clone();
    let st_clone = clean_stage.clone();
    let tl_clone = clean_tail.clone();
    let rev = input.expected_revision;

    let (lease_until, revoked) = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let perms = automation::read_permissions_sync(&tx, &uid)?;
            let revoked = automation::revoke_disallowed_jobs_sync(&tx, &uid, &perms)?;
            let now = now_unix();
            let until = now + LEASE_TTL_SECS;
            let changed = tx.execute(
                "UPDATE node_jobs
                 SET lease_until = ?6, stage = ?7, log_tail = ?8, updated_at = ?9
                 WHERE task_id = ?1 AND user_id = ?2 AND node_id = ?3
                   AND lease_hash = ?4 AND task_revision = ?5
                   AND status = 'running' AND lease_until > ?9",
                params![tid, uid, node_id, lease_hash, rev, until, st_clone, tl_clone, now],
            )?;
            tx.commit()?;
            Ok(((changed == 1).then_some(until), revoked))
        })
        .await?;

    emit_failed_tasks(&hub, revoked);
    let Some(lease_until) = lease_until else {
        return Err(Error(StatusCode::CONFLICT, "node lease is no longer valid"));
    };

    hub.events.emit(
        user_id,
        "task",
        json!({
            "task_id": task_id,
            "status": "running",
            "stage": clean_stage,
            "log_tail": clean_tail,
            "elapsed_secs": input.elapsed_secs,
        }),
    );

    Ok(Json(json!({"ok": true, "lease_until": lease_until})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitAttachment {
    name: String,
    mime_type: String,
    data_base64: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmitResultInput {
    lease_token: String,
    request_id: String,
    expected_revision: i64,
    status: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    attachments: Vec<SubmitAttachment>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    agent_session_id: Option<String>,
    #[serde(default)]
    backend: Option<String>,
    #[serde(default)]
    blocked_reason: Option<String>,
}

fn valid_session_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() >= 4
        && trimmed.len() <= 128
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn sanitize_backend_name(raw: &str) -> Option<String> {
    let norm = raw.trim().to_ascii_lowercase();
    if !norm.is_empty()
        && norm.len() <= 32
        && norm
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        Some(norm)
    } else {
        None
    }
}

pub async fn submit_result(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<SubmitResultInput>,
) -> Result<Json<Value>> {
    let (user_id, node_id, node_name) = authenticate_node(&hub, &headers).await?;
    if !matches!(input.status.as_str(), "completed" | "failed") {
        return Err(invalid("status must be completed or failed"));
    }
    if input.request_id.is_empty() || input.request_id.len() > 128 {
        return Err(invalid("invalid request_id"));
    }
    let submission_hash = sha256_hex(&format!(
        "{}:{}:{}",
        input.request_id, input.status, input.content
    ));
    let lease_hash = sha256_hex(&input.lease_token);

    let final_status = if input.status == "completed" {
        "done"
    } else {
        "failed"
    };

    // Pre-validate job revision, idempotent replay, and active lease BEFORE writing any evidence
    // attachments to disk/DB, preventing duplicate files on replay or orphaned files on 409.
    let requester_id: String = {
        let tid = task_id.clone();
        let uid = user_id.clone();
        let nid = node_id.clone();
        let rev = input.expected_revision;
        let sub_hash = submission_hash.clone();
        let l_hash = lease_hash.clone();
        let pre = hub
            .db
            .read(move |conn| {
                struct PreCheckRow {
                    requester_id: String,
                    status: String,
                    lease_hash: Option<String>,
                    lease_until: Option<i64>,
                    submission_hash: Option<String>,
                }
                let row: Option<PreCheckRow> = conn
                    .query_row(
                        "SELECT t.requester_id, j.status, j.lease_hash, j.lease_until, j.submission_hash
                         FROM tasks t
                         LEFT JOIN node_jobs j
                           ON j.task_id = t.id AND j.user_id = ?2 AND j.node_id = ?3 AND j.task_revision = ?4
                         WHERE t.id = ?1 AND t.owner_id = ?2",
                        params![tid, uid, nid, rev],
                        |r| {
                            Ok(PreCheckRow {
                                requester_id: r.get(0)?,
                                status: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                                lease_hash: r.get(2)?,
                                lease_until: r.get(3)?,
                                submission_hash: r.get(4)?,
                            })
                        },
                    )
                    .optional()?;
                let Some(PreCheckRow {
                    requester_id: req_id,
                    status: cur_status,
                    lease_hash: stored_lease,
                    lease_until,
                    submission_hash: stored_sub,
                }) = row
                else {
                    return Err(denied());
                };
                if cur_status.is_empty() {
                    return Err(Error(
                        StatusCode::CONFLICT,
                        "node job not found for revision",
                    ));
                }
                if stored_sub.as_deref() == Some(sub_hash.as_str()) {
                    return Ok(None);
                }
                let now = now_unix();
                if cur_status != "running"
                    || stored_lease.as_deref() != Some(l_hash.as_str())
                    || lease_until.is_none_or(|until| until <= now)
                {
                    return Err(Error(
                        StatusCode::CONFLICT,
                        "node lease expired or superseded",
                    ));
                }
                Ok(Some(req_id))
            })
            .await?;
        match pre {
            None => {
                return Ok(Json(
                    json!({"ok": true, "task_id": task_id, "status": final_status}),
                ));
            }
            Some(req_id) => req_id,
        }
    };

    let sanitized_content = sanitize_terminal_text(&input.content);
    let mut content = truncate_bytes(sanitized_content.trim(), 16_000);
    let extra_grants = if requester_id != user_id {
        vec![requester_id.clone()]
    } else {
        Vec::new()
    };
    for att in input.attachments.iter().take(4) {
        if let Ok(meta) = attachments::store_bytes(
            &hub,
            &user_id,
            &att.name,
            &att.mime_type,
            &att.data_base64,
            &extra_grants,
        )
        .await
        {
            if meta.mime_type.starts_with("image/") {
                content.push_str(&format!(
                    "\n\n![{}](/api/attachments/{})",
                    meta.name, meta.id
                ));
            } else {
                content.push_str(&format!(
                    "\n\n[{}](/api/attachments/{})",
                    meta.name, meta.id
                ));
            }
        }
    }

    let uid = user_id.clone();
    let nid = node_id.clone();
    let tid = task_id.clone();
    let job_status = input.status.clone();
    let content_db = content.clone();
    let rev = input.expected_revision;
    let node_name_db = node_name.clone();
    let agent_sid_db = input.agent_session_id.as_deref().and_then(valid_session_id);
    let submit_backend = input.backend.as_deref().and_then(sanitize_backend_name);
    let submit_backend_db = submit_backend.clone();
    let blocked_reason = if final_status == "failed" {
        input
            .blocked_reason
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| truncate_bytes(s, 240))
            .unwrap_or_default()
    } else {
        String::new()
    };
    let blocked_db = blocked_reason.clone();
    let submit_agent_db = input
        .agent
        .as_deref()
        .and_then(normalize_agent_name)
        .unwrap_or("")
        .to_owned();

    let req_for_tx = requester_id.clone();
    let (
        title,
        node_label,
        is_cross_user,
        auto_deliver,
        already_submitted,
        task_thread_id,
        on_complete,
        cb_depth,
        will_callback,
    ) = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let perms = automation::read_permissions_sync(&tx, &uid)?;
            let now = now_unix();
            struct JobRow {
                status: String,
                agent: String,
                role: String,
                model: String,
                lease_hash: Option<String>,
                lease_until: Option<i64>,
                submission_hash: Option<String>,
            }
            let job: Option<JobRow> = tx
                .query_row(
                    "SELECT status, agent, role, model, lease_hash, lease_until, submission_hash
                     FROM node_jobs
                     WHERE task_id = ?1 AND user_id = ?2 AND node_id = ?3 AND task_revision = ?4",
                    params![tid, uid, nid, rev],
                    |r| {
                        Ok(JobRow {
                            status: r.get(0)?,
                            agent: r.get(1)?,
                            role: r.get(2)?,
                            model: r.get(3)?,
                            lease_hash: r.get(4)?,
                            lease_until: r.get(5)?,
                            submission_hash: r.get(6)?,
                        })
                    },
                )
                .optional()?;
            let Some(JobRow {
                status: cur_status,
                agent: job_agent,
                role: job_role,
                model: job_model,
                lease_hash: stored_lease,
                lease_until,
                submission_hash: stored_sub,
            }) = job
            else {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "node job not found for revision",
                ));
            };
            // Idempotent replay check for journaled submissions.
            if stored_sub.as_deref() == Some(submission_hash.as_str()) {
                let title: String =
                    tx.query_row("SELECT title FROM tasks WHERE id=?1", [&tid], |r| r.get(0))?;
                tx.commit()?;
                return Ok((
                    title,
                    node_name_db,
                    req_for_tx != uid,
                    perms.auto_deliver,
                    true,
                    None,
                    String::new(),
                    0,
                    false,
                ));
            }
            if cur_status != "running"
                || stored_lease.as_deref() != Some(lease_hash.as_str())
                || lease_until.is_none_or(|until| until <= now)
            {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "node lease expired or superseded",
                ));
            }
            let actual_agent = if !submit_agent_db.is_empty() {
                submit_agent_db
            } else {
                job_agent.trim().to_ascii_lowercase()
            };
            let role_trim = job_role.trim();
            let model_trim = job_model.trim();
            tx.execute(
                "UPDATE node_jobs
                 SET status = ?2, submission_hash = ?3, stage = ?4, updated_at = ?5
                 WHERE task_id = ?1",
                params![tid, job_status, submission_hash, final_status, now],
            )?;
            let (title, req_id, task_thread_id, on_complete, cb_depth): (
                String,
                String,
                Option<String>,
                String,
                i64,
            ) = tx.query_row(
                "SELECT title, requester_id, thread_id, on_complete, callback_depth FROM tasks WHERE id = ?1",
                [&tid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?;
            let will_callback = cb_depth < crate::brain::MAX_CALLBACK_DEPTH
                && (!on_complete.trim().is_empty() || crate::brain::should_auto_digest(&content_db));
            tx.execute(
                "UPDATE tasks
                 SET status = ?2, result = ?3, blocked_reason = ?5, revision = revision + 1, updated_at = ?4
                 WHERE id = ?1",
                params![tid, final_status, content_db, now, blocked_db],
            )?;
            // Post raw result to chat immediately only when there is no automatic callback;
            // when `on_complete` triggers, keep the raw output in tasks.result / drawer and let
            // the brain post a single concise digest (falling back to raw if the callback fails).
            let mut label_parts: Vec<&str> = Vec::new();
            if !role_trim.is_empty() {
                label_parts.push(role_trim);
            }
            if !actual_agent.is_empty() {
                label_parts.push(actual_agent.as_str());
            }
            if !model_trim.is_empty() {
                label_parts.push(model_trim);
            }
            let node_label = if label_parts.is_empty() {
                node_name_db.clone()
            } else {
                format!("{node_name_db} ({})", label_parts.join(" / "))
            };
            if !will_callback {
                let msg_body = format!("**[节点 {node_label} · {title}]**\n\n{content_db}");
                tx.execute(
                    "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                     VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                    params![uid, task_thread_id, msg_body, now],
                )?;
            }
            // Update implicit thread metadata & persist (role, engine, model)-bound CLI session ID.
            if let Some(ref thr_id) = task_thread_id {
                let compact_outcome = truncate_bytes(
                    &content_db
                        .lines()
                        .map(str::trim)
                        .filter(|l| !l.is_empty())
                        .take(3)
                        .collect::<Vec<_>>()
                        .join(" "),
                    220,
                );
                tx.execute(
                    "UPDATE threads
                     SET node_id = ?3,
                         node_session_id = CASE WHEN ?4 IS NOT NULL THEN ?4 ELSE node_session_id END,
                         node_session_agent = CASE WHEN ?4 IS NOT NULL THEN ?5 ELSE node_session_agent END,
                         node_session_role = CASE WHEN ?4 IS NOT NULL THEN ?6 ELSE node_session_role END,
                         node_session_model = CASE WHEN ?4 IS NOT NULL THEN ?7 ELSE node_session_model END,
                         summary = CASE WHEN ?8 != '' THEN ?8 ELSE summary END,
                         status = 'active',
                         turn_count = turn_count + 1,
                         updated_at = ?9
                     WHERE id = ?1 AND user_id = ?2",
                    params![
                        thr_id,
                        uid,
                        nid,
                        agent_sid_db,
                        actual_agent,
                        role_trim,
                        model_trim,
                        compact_outcome,
                        now
                    ],
                )?;
            }
            // If cross-user task and owner opted into auto_deliver, share result to requester too.
            if req_id != uid && perms.auto_deliver && final_status == "done" {
                let peer_msg = format!("**[协作交付 · {title}]**\n\n{content_db}");
                tx.execute(
                    "INSERT INTO messages(user_id, peer_id, role, content, status, created_at)
                     VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                    params![req_id, uid, peer_msg, now],
                )?;
            }
            let audit_summary = match submit_backend_db.as_deref() {
                Some(b) => format!("{node_name_db} [{b}]: {title}"),
                None => format!("{node_name_db}: {title}"),
            };
            record_audit(
                &tx,
                &uid,
                "node_submit",
                final_status,
                &audit_summary,
                &truncate_bytes(&content_db, 240),
            )?;
            tx.commit()?;
            Ok((
                title,
                node_label,
                req_id != uid,
                perms.auto_deliver,
                false,
                task_thread_id,
                on_complete,
                cb_depth,
                will_callback,
            ))
        })
        .await?;

    if !already_submitted {
        let mut task_event = json!({
            "task_id": task_id,
            "status": final_status,
            "result": content,
            "blocked_reason": blocked_reason,
        });
        if let Some(ref backend) = submit_backend {
            task_event["backend"] = json!(backend);
        }
        hub.events.emit(user_id.clone(), "task", task_event);
        if will_callback {
            hub.events.emit(
                user_id.clone(),
                "stage",
                json!({"stage": "节点已完成，正在提炼结论…"}),
            );
        } else {
            hub.events
                .emit(user_id.clone(), "message", json!({"task_id": task_id}));
        }
        if is_cross_user && auto_deliver && final_status == "done" {
            hub.events
                .emit(requester_id.clone(), "message", json!({"task_id": task_id}));
        }
        // Trigger optional GitLab reply, IM notification, and brain completion callback asynchronously.
        let h_bg = hub.clone();
        let tid_bg = task_id.clone();
        let uid_bg = user_id.clone();
        let title_bg = title.clone();
        let content_bg = content.clone();
        let status_bg = final_status.to_owned();
        tokio::spawn(async move {
            crate::integrations::gitlab::maybe_reply_task(&h_bg, &tid_bg, &node_name, &content_bg)
                .await;
            if will_callback {
                crate::brain::run_task_callback(
                    h_bg,
                    uid_bg,
                    tid_bg,
                    title_bg,
                    node_label,
                    status_bg,
                    content_bg,
                    on_complete,
                    task_thread_id,
                    cb_depth,
                )
                .await;
            } else {
                crate::integrations::im::maybe_notify(
                    &h_bg,
                    &uid_bg,
                    "delivery",
                    &format!("任务完成：{title_bg}"),
                    &content_bg,
                )
                .await;
            }
        });
    }

    Ok(Json(
        json!({"ok": true, "task_id": task_id, "status": final_status}),
    ))
}

pub async fn cancel_task(hub: &Hub, user: &User, task_id: &str) -> Result<Json<Value>> {
    let uid = user.id.clone();
    let tid = task_id.to_owned();
    let (owner_id, requester_id) = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = now_unix();
            let parties: Option<(String, String)> = tx
                .query_row(
                    "SELECT owner_id, requester_id FROM tasks
                     WHERE id=?1 AND (owner_id=?2 OR requester_id=?2) AND status IN ('pending','running')",
                    params![tid, uid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((owner_id, requester_id)) = parties else {
                return Err(denied());
            };
            tx.execute(
                "UPDATE tasks SET status='canceled', revision=revision+1, updated_at=?2
                 WHERE id=?1",
                params![tid, now],
            )?;
            tx.execute(
                "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?2
                 WHERE task_id=?1 AND status IN ('queued','running')",
                params![tid, now],
            )?;
            tx.commit()?;
            Ok((owner_id, requester_id))
        })
        .await?;
    hub.events.emit(
        &owner_id,
        "task",
        json!({"task_id": task_id, "status": "canceled"}),
    );
    if requester_id != owner_id {
        hub.events.emit(
            &requester_id,
            "task",
            json!({"task_id": task_id, "status": "canceled"}),
        );
    }
    Ok(Json(json!({"ok": true, "task_id": task_id})))
}

pub async fn agent_script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/x-python; charset=utf-8")],
        include_str!("../../node/agenct-node.py"),
    )
}

pub async fn install_sh(State(hub): State<Hub>) -> impl IntoResponse {
    let origin = &hub.config.public_origin;
    let body = format!(
        r#"#!/usr/bin/env sh
set -eu
HUB="{origin}"
TOKEN="${{1:-${{AGENCT_NODE_TOKEN:-}}}}"
if [ -z "$TOKEN" ]; then
  echo "Usage: curl -fsSL $HUB/node/install.sh | sh -s -- <agenct_node_token> [node-args...]" >&2
  exit 2
fi
if [ "$#" -gt 0 ]; then
  shift
fi
DIR="$HOME/.agenct"
mkdir -p "$DIR" && chmod 700 "$DIR"
curl -fsSL "$HUB/node/agenct-node.py" -o "$DIR/agenct-node.py"
chmod 700 "$DIR/agenct-node.py"
exec python3 "$DIR/agenct-node.py" --hub "$HUB" --token "$TOKEN" "$@"
"#
    );
    (
        [(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")],
        body,
    )
}

pub async fn install_ps1(State(hub): State<Hub>) -> impl IntoResponse {
    let origin = &hub.config.public_origin;
    let body = format!(
        r#"$ErrorActionPreference = "Stop"
$dir = Join-Path $HOME ".agenct"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$script = Join-Path $dir "agenct-node.py"
Invoke-WebRequest -UseBasicParsing -Uri "{origin}/node/agenct-node.py" -OutFile $script
Write-Host "Downloaded $script. Run: python `"$script`" --hub `"{origin}`" --token <YOUR_TOKEN>"
"#
    );
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

#[cfg(test)]
mod tests {
    use super::{
        extract_node_repos, pick_node_for_task_sync, pick_node_for_task_with_agent_sync,
        resolve_node_agents, sanitize_terminal_text,
    };
    use crate::config::now_unix;
    use rusqlite::{params, Connection};

    #[test]
    fn strips_ansi_csi_osc_and_control_bytes_preserving_utf8() {
        let raw = "\x1b[0m\n\x1b[32;1m活跃会话\x1b[0m\r\n\x1b]0;window title\x07- `agy`: 3 windows\n\t- `cc`: 5 windows\x00";
        assert_eq!(
            sanitize_terminal_text(raw),
            "\n活跃会话\n- `agy`: 3 windows\n\t- `cc`: 5 windows"
        );
    }

    #[test]
    fn extracts_repos_and_routes_online_node_by_repo_name() {
        assert_eq!(
            extract_node_repos("kind:pc | os:linux | agent:codex | repos:api,agenct,web-app"),
            vec!["api", "agenct", "web-app"]
        );

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE nodes (
                id TEXT NOT NULL, user_id TEXT NOT NULL, name TEXT NOT NULL,
                kind TEXT NOT NULL, capabilities TEXT NOT NULL,
                default_agent TEXT NOT NULL DEFAULT '',
                allowed_agents TEXT NOT NULL DEFAULT '',
                token_hash TEXT NOT NULL, last_seen_at INTEGER, created_at INTEGER NOT NULL
            );",
        )
        .unwrap();
        let now = now_unix();
        conn.execute(
            "INSERT INTO nodes VALUES ('n-front', 'u1', 'Front', 'pc', 'kind:pc | agent:claude | agents:claude,opencode | repos:frontend,api', '', '', 'h1', ?1, 10)",
            params![now],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes VALUES ('n-back', 'u1', 'Back', 'pc', 'kind:pc | agent:opencode | agents:codex,opencode | repos:agenct,billing-api', '', '', 'h2', ?1, 20)",
            params![now],
        ).unwrap();
        conn.execute(
            "INSERT INTO nodes VALUES ('n-off', 'u1', 'Offline', 'pc', 'kind:pc | repos:secret-proj', '', '', 'h3', ?1, 30)",
            params![now - 120],
        ).unwrap();

        // Word-boundary prevents "api" from matching "rapid", while "agenct" routes to n-back.
        let picked = pick_node_for_task_sync(
            &conn,
            "u1",
            None,
            false,
            Some("Check rapid growth and fix agenct repo"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(picked.id, "n-back");

        // Engine constraint overrides default routing so claude hits n-front even if prompt mentions agenct.
        let picked_claude = pick_node_for_task_with_agent_sync(
            &conn,
            "u1",
            None,
            false,
            Some("Fix agenct repo"),
            Some("claude"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(picked_claude.id, "n-front");

        // Offline node is skipped even if its repo matches.
        let picked_online =
            pick_node_for_task_sync(&conn, "u1", None, false, Some("Check secret-proj"))
                .unwrap()
                .unwrap();
        assert_eq!(picked_online.id, "n-front");

        // Explicit preferred_id overrides repo matching.
        let picked_explicit =
            pick_node_for_task_sync(&conn, "u1", Some("Front"), false, Some("Fix agenct repo"))
                .unwrap()
                .unwrap();
        assert_eq!(picked_explicit.id, "n-front");
    }

    #[test]
    fn resolves_effective_agents_with_hub_allowlist_and_default_override() {
        let caps = "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode";
        let (def, eff, det) = resolve_node_agents("", "", caps);
        assert_eq!(def, "opencode");
        assert_eq!(det, vec!["claude", "codex", "agy", "opencode"]);
        assert_eq!(eff, det);

        let (def2, eff2, _) = resolve_node_agents("claude", "claude,opencode", caps);
        assert_eq!(def2, "claude");
        assert_eq!(eff2, vec!["claude", "opencode"]);

        // Disjoint Hub allowlist rejects unmatched engines safely.
        let (def3, eff3, _) = resolve_node_agents("", "ollama", caps);
        assert_eq!(def3, "");
        assert!(eff3.is_empty());
    }
}
