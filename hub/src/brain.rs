//! Unified single-brain agent loop (SOUL + MEMORY + Ledgers + Tools) & bootstrap API.
use crate::{
    automation, collab,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid,
    llm::ChatMessage,
    nodes, Error, Hub, Result,
};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
use uuid::Uuid;

const DEFAULT_AGENT_NAME: &str = "Agenct";
const DEFAULT_SOUL_MD: &str = "- 身份：你的 7×24h 个人超级智能体 Agenct。\n- 风格：干练、直接、结论先行，用第一人称自然沟通。\n- 原则：云端能直接解决的直接解决；涉及本机代码或终端时调度在线节点；跨人承诺或高危操作主动说明边界。";
const DEFAULT_MEMORY_MD: &str =
    "暂无长期记忆。在对话中告诉我你的习惯、规矩或环境事实，我会自动整理记住。";
const THREAD_IDLE_ARCHIVE_SECS: i64 = 48 * 3600;
const COMPACT_KEEP_RECENT: usize = 8;
const COMPACT_MIN_BATCH: usize = 10;
const COMPACT_MAX_BATCH: usize = 24;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadView {
    pub id: String,
    pub title: String,
    pub status: String,
    pub summary: String,
    pub node_id: Option<String>,
    pub has_session: bool,
    pub turn_count: i64,
    pub updated_at: i64,
}

const DEFAULT_AVATAR_ID: &str = "agenct";

fn normalize_avatar_id(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "agenct" | "default" | "默认" => Some("agenct"),
        "sakura" | "樱粉" | "樱粉巫女" | "八重神子" => Some("sakura"),
        "violet" | "冷紫" | "冷紫和风" | "雷电将军" => Some("violet"),
        "cheshire" | "柴郡" | "猫耳女仆" => Some("cheshire"),
        _ => None,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub soul_md: String,
    pub memory_md: String,
    pub avatar_id: String,
    pub updated_at: i64,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            name: DEFAULT_AGENT_NAME.into(),
            soul_md: DEFAULT_SOUL_MD.into(),
            memory_md: DEFAULT_MEMORY_MD.into(),
            avatar_id: DEFAULT_AVATAR_ID.into(),
            updated_at: 0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Ledger {
    pub id: String,
    pub title: String,
    pub status: String,
    pub notes: String,
    pub updated_at: i64,
}

pub fn read_profile_sync(conn: &Connection, user_id: &str) -> Result<AgentProfile> {
    Ok(conn
        .query_row(
            "SELECT name, soul_md, memory_md, avatar_id, updated_at FROM agent_profiles WHERE user_id = ?1",
            [user_id],
            |r| {
                let raw_avatar: String = r.get(3)?;
                Ok(AgentProfile {
                    name: r.get(0)?,
                    soul_md: r.get(1)?,
                    memory_md: r.get(2)?,
                    avatar_id: normalize_avatar_id(&raw_avatar)
                        .unwrap_or(DEFAULT_AVATAR_ID)
                        .to_owned(),
                    updated_at: r.get(4)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

pub fn list_ledgers_sync(conn: &Connection, user_id: &str) -> Result<Vec<Ledger>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, status, notes, updated_at
         FROM ledgers WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT 50",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        Ok(Ledger {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            notes: r.get(3)?,
            updated_at: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn read_compaction_state_sync(conn: &Connection, user_id: &str) -> Result<(String, i64)> {
    Ok(conn
        .query_row(
            "SELECT rolling_summary, compacted_msg_id FROM agent_profiles WHERE user_id = ?1",
            [user_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or_else(|| (String::new(), 0)))
}

pub fn list_active_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let cutoff = now_unix().saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1 AND status = 'active' AND updated_at >= ?2
         ORDER BY updated_at DESC LIMIT 6",
    )?;
    let rows = stmt.query_map(params![user_id, cutoff], |r| {
        let sid: Option<String> = r.get(5)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            node_id: r.get(4)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn list_threads_sync(conn: &Connection, user_id: &str) -> Result<Vec<ThreadView>> {
    let mut stmt = conn.prepare(
        "SELECT id, title, status, summary, node_id, node_session_id, turn_count, updated_at
         FROM threads
         WHERE user_id = ?1
         ORDER BY CASE status WHEN 'active' THEN 0 ELSE 1 END, updated_at DESC
         LIMIT 30",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let sid: Option<String> = r.get(5)?;
        Ok(ThreadView {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            summary: r.get(3)?,
            node_id: r.get(4)?,
            has_session: sid.as_deref().is_some_and(|s| !s.is_empty()),
            turn_count: r.get(6)?,
            updated_at: r.get(7)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Single initial payload for the frontend (replaces 7 parallel polling requests).
pub async fn bootstrap(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();

    let data = hub
        .db
        .read(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let threads = list_threads_sync(conn, &uid)?;
            let ledgers = list_ledgers_sync(conn, &uid)?;
            let node_list = nodes::list_nodes_sync(conn, &uid)?;
            let permissions = automation::read_permissions_sync(conn, &uid)?;
            let rules = automation::list_rules_sync(conn, &uid)?;
            let crons = automation::list_crons_sync(conn, &uid)?;

            let mut msg_stmt = conn.prepare(
                "SELECT id, peer_id, role, content, status, created_at FROM (
                    SELECT id, peer_id, role, content, status, created_at
                    FROM messages WHERE user_id = ?1
                    ORDER BY id DESC LIMIT 60
                 ) ORDER BY id ASC",
            )?;
            let messages: Vec<Value> = msg_stmt
                .query_map([&uid], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "peer_id": r.get::<_, Option<String>>(1)?,
                        "role": r.get::<_, String>(2)?,
                        "content": r.get::<_, String>(3)?,
                        "status": r.get::<_, String>(4)?,
                        "created_at": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut task_stmt = conn.prepare(
                "SELECT t.id, t.requester_id, t.owner_id, t.thread_id, t.title, t.instruction, t.status,
                        t.result, t.revision, t.updated_at,
                        j.node_id, j.status, j.stage, j.log_tail
                 FROM tasks t
                 LEFT JOIN node_jobs j ON j.task_id = t.id
                 WHERE t.owner_id = ?1 OR t.requester_id = ?1
                 ORDER BY t.updated_at DESC LIMIT 40",
            )?;
            let tasks: Vec<Value> = task_stmt
                .query_map([&uid], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "requester_id": r.get::<_, String>(1)?,
                        "owner_id": r.get::<_, String>(2)?,
                        "thread_id": r.get::<_, Option<String>>(3)?,
                        "title": r.get::<_, String>(4)?,
                        "instruction": r.get::<_, String>(5)?,
                        "status": r.get::<_, String>(6)?,
                        "result": r.get::<_, Option<String>>(7)?,
                        "revision": r.get::<_, i64>(8)?,
                        "updated_at": r.get::<_, i64>(9)?,
                        "node_id": r.get::<_, Option<String>>(10)?,
                        "job_status": r.get::<_, Option<String>>(11)?,
                        "stage": r.get::<_, Option<String>>(12)?,
                        "log_tail": r.get::<_, Option<String>>(13)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut audit_stmt = conn.prepare(
                "SELECT id, action, status, title, detail, created_at
                 FROM audit_logs WHERE user_id = ?1 ORDER BY id DESC LIMIT 40",
            )?;
            let audit_logs: Vec<Value> = audit_stmt
                .query_map([&uid], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "action": r.get::<_, String>(1)?,
                        "status": r.get::<_, String>(2)?,
                        "title": r.get::<_, String>(3)?,
                        "detail": r.get::<_, String>(4)?,
                        "created_at": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let mut contact_stmt = conn.prepare(
                "SELECT id, username, display_name FROM users
                 WHERE status='active' AND id != ?1 ORDER BY username ASC",
            )?;
            let contacts: Vec<Value> = contact_stmt
                .query_map([&uid], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "username": r.get::<_, String>(1)?,
                        "display_name": r.get::<_, String>(2)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            Ok(json!({
                "profile": profile,
                "threads": threads,
                "messages": messages,
                "tasks": tasks,
                "nodes": node_list,
                "ledgers": ledgers,
                "permissions": permissions,
                "rules": rules,
                "crons": crons,
                "audit_logs": audit_logs,
                "contacts": contacts,
            }))
        })
        .await?;

    let mut out = data;
    out["user"] = json!(user);
    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveProfileInput {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub soul_md: Option<String>,
    #[serde(default)]
    pub memory_md: Option<String>,
    #[serde(default)]
    pub avatar_id: Option<String>,
}

pub async fn save_profile(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveProfileInput>,
) -> Result<Json<AgentProfile>> {
    let user = hub.require_user(&headers).await?;
    let next_avatar = match input.avatar_id.as_deref() {
        Some(raw) => Some(
            normalize_avatar_id(raw)
                .ok_or_else(|| invalid("invalid avatar_id"))?
                .to_owned(),
        ),
        None => None,
    };
    let next_name = match input.name {
        Some(raw) => {
            let trimmed = truncate_bytes(raw.trim(), 48);
            if trimmed.is_empty() {
                return Err(invalid("agent name cannot be empty"));
            }
            Some(trimmed)
        }
        None => None,
    };
    let next_soul = input.soul_md.map(|s| truncate_bytes(s.trim(), 6000));
    let next_mem = input.memory_md.map(|s| truncate_bytes(s.trim(), 8000));
    let uid = user.id.clone();

    let profile = hub
        .db
        .write(move |conn| {
            let mut p = read_profile_sync(conn, &uid)?;
            if let Some(n) = next_name {
                p.name = n;
            }
            if let Some(s) = next_soul {
                p.soul_md = s;
            }
            if let Some(m) = next_mem {
                p.memory_md = m;
            }
            if let Some(a) = next_avatar {
                p.avatar_id = a;
            }
            p.updated_at = now_unix();
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, avatar_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id) DO UPDATE SET
                   name=excluded.name, soul_md=excluded.soul_md,
                   memory_md=excluded.memory_md, avatar_id=excluded.avatar_id,
                   updated_at=excluded.updated_at",
                params![uid, p.name, p.soul_md, p.memory_md, p.avatar_id, p.updated_at],
            )?;
            Ok(p)
        })
        .await?;
    hub.events
        .emit(user.id, "brain", json!({"profile": profile}));
    Ok(Json(profile))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpsertLedgerInput {
    #[serde(default)]
    pub id: Option<String>,
    pub title: String,
    #[serde(default = "default_active")]
    pub status: String,
    #[serde(default)]
    pub notes: String,
}

fn default_active() -> String {
    "active".into()
}

pub async fn upsert_ledger(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<UpsertLedgerInput>,
) -> Result<Json<Ledger>> {
    let user = hub.require_user(&headers).await?;
    let title = truncate_bytes(input.title.trim(), 96);
    if title.is_empty() {
        return Err(invalid("ledger title required"));
    }
    if !matches!(input.status.as_str(), "active" | "muted" | "done") {
        return Err(invalid("ledger status must be active, muted, or done"));
    }
    let ledger = Ledger {
        id: input
            .id
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("ldg_{}", &Uuid::new_v4().simple().to_string()[..8])),
        title,
        status: input.status,
        notes: truncate_bytes(input.notes.trim(), 6000),
        updated_at: now_unix(),
    };
    let l_db = ledger.clone();
    let uid = user.id.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO ledgers(id, user_id, title, status, notes, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   title=excluded.title, status=excluded.status,
                   notes=excluded.notes, updated_at=excluded.updated_at
                 WHERE ledgers.user_id = ?2",
                params![
                    l_db.id,
                    uid,
                    l_db.title,
                    l_db.status,
                    l_db.notes,
                    l_db.updated_at
                ],
            )?;
            Ok(())
        })
        .await?;
    hub.events.emit(user.id, "brain", json!({"ledger": ledger}));
    Ok(Json(ledger))
}

pub async fn delete_ledger(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();
    let lid = id.clone();
    hub.db
        .write(move |conn| {
            if conn.execute(
                "DELETE FROM ledgers WHERE id = ?1 AND user_id = ?2",
                params![lid, uid],
            )? == 0
            {
                return Err(denied());
            }
            Ok(())
        })
        .await?;
    hub.events
        .emit(user.id, "brain", json!({"deleted_ledger": id}));
    Ok(Json(json!({"ok": true, "id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateThreadInput {
    pub status: String,
}

pub async fn update_thread(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<UpdateThreadInput>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let status = input.status.trim().to_owned();
    if !matches!(status.as_str(), "active" | "archived") {
        return Err(invalid("thread status must be active or archived"));
    }
    let uid = user.id.clone();
    let tid = id.clone();
    let st_db = status.clone();
    hub.db
        .write(move |conn| {
            if conn.execute(
                "UPDATE threads SET status = ?3, updated_at = ?4 WHERE id = ?1 AND user_id = ?2",
                params![tid, uid, st_db, now_unix()],
            )? == 0
            {
                return Err(denied());
            }
            Ok(())
        })
        .await?;
    hub.events.emit(
        user.id,
        "brain",
        json!({"updated_thread": id, "status": status}),
    );
    Ok(Json(json!({"ok": true, "id": id, "status": status})))
}

pub async fn delete_thread(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();
    let tid = id.clone();
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            if tx.execute(
                "DELETE FROM threads WHERE id = ?1 AND user_id = ?2",
                params![tid, uid],
            )? == 0
            {
                return Err(denied());
            }
            tx.execute(
                "UPDATE messages SET thread_id = NULL WHERE thread_id = ?1 AND user_id = ?2",
                params![tid, uid],
            )?;
            tx.execute(
                "UPDATE tasks SET thread_id = NULL WHERE thread_id = ?1 AND owner_id = ?2",
                params![tid, uid],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user.id, "brain", json!({"deleted_thread": id}));
    Ok(Json(json!({"ok": true, "id": id})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatInput {
    pub message: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Deserialize)]
struct BrainResponse {
    #[serde(default)]
    reply: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    thread_title: Option<String>,
    #[serde(default)]
    tools: Vec<ToolCall>,
}

#[derive(Deserialize)]
struct ToolCall {
    tool: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    avatar_id: Option<String>,
    #[serde(default)]
    soul_md: Option<String>,
    #[serde(default)]
    memory_append: Option<String>,
    #[serde(default)]
    memory_replace: Option<String>,
    #[serde(default)]
    ledger_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    notes_append: Option<String>,
    #[serde(default)]
    notes_replace: Option<String>,
    #[serde(default)]
    cron_id: Option<String>,
    #[serde(default)]
    daily_time_cst: Option<String>,
    #[serde(default)]
    interval_minutes: Option<i64>,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    rule_id: Option<String>,
    #[serde(default)]
    keyword: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    prefer_cloud: Option<bool>,
    #[serde(default)]
    peer: Option<String>,
}

pub async fn chat(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<ChatInput>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let msg = input.message.trim();
    if msg.is_empty() || msg.len() > 8000 {
        return Err(invalid("message must be 1..8000 bytes"));
    }
    if let Some(ref rid) = input.request_id {
        if rid.is_empty()
            || rid.len() > 128
            || !rid
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(invalid("invalid request_id"));
        }
    }
    let out = run_turn(&hub, &user.id, msg, input.request_id.as_deref()).await?;
    Ok(Json(out))
}

/// Maximum LLM calls per chat turn (initial call + tool-result follow-ups).
const MAX_AGENT_STEPS: usize = 3;
/// Per-tool observation budget fed back into the model.
const TOOL_OBSERVATION_MAX_BYTES: usize = 4000;

/// State of a previously seen client `request_id`.
enum RequestState {
    New,
    Replay(String),
    InFlight,
    Failed,
}

async fn lookup_request(hub: &Hub, user_id: &str, request_id: &str) -> Result<RequestState> {
    let uid = user_id.to_owned();
    let rid = request_id.to_owned();
    hub.db
        .read(move |conn| {
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT id, status FROM messages WHERE user_id=?1 AND request_id=?2",
                    params![uid, rid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((mid, status)) = row else {
                return Ok(RequestState::New);
            };
            match status.as_str() {
                "pending" => Ok(RequestState::InFlight),
                "failed" => Ok(RequestState::Failed),
                _ => {
                    let reply: Option<String> = conn
                        .query_row(
                            "SELECT content FROM messages
                             WHERE user_id=?1 AND reply_to=?2 AND role='assistant'
                             ORDER BY id ASC LIMIT 1",
                            params![uid, mid],
                            |r| r.get(0),
                        )
                        .optional()?;
                    // A completed request without a linked reply predates schema v4; never guess.
                    Ok(reply.map_or(RequestState::InFlight, RequestState::Replay))
                }
            }
        })
        .await
}

/// Everything the brain needs for one turn, loaded in a single DB transaction.
struct TurnSnapshot {
    profile: AgentProfile,
    rolling_summary: String,
    threads: Vec<ThreadView>,
    ledgers: Vec<Ledger>,
    nodes: Vec<nodes::NodeView>,
    crons: Vec<automation::CronJob>,
    rules: Vec<automation::DispatchRule>,
    matched_rule: Option<automation::DispatchRule>,
    gitlab_configured: bool,
    history: Vec<ChatMessage>,
    contacts: Vec<String>,
    user_msg_id: i64,
}

/// Unified single-brain turn execution used by both Web UI and inbound IM webhook.
///
/// Lifecycle of the user message: inserted as `pending` → `complete` together with the
/// assistant reply (linked via `reply_to`), or `failed` if the turn cannot produce a reply.
/// A failed `request_id` may be retried; a completed one is replayed idempotently.
pub async fn run_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    request_id: Option<&str>,
) -> Result<Value> {
    if let Some(rid) = request_id {
        match lookup_request(hub, user_id, rid).await? {
            RequestState::Replay(reply) => return Ok(json!({"reply": reply, "replayed": true})),
            RequestState::InFlight => {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "request is already being processed",
                ))
            }
            RequestState::New | RequestState::Failed => {}
        }
    }

    // Per-user turn serialization gate.
    let gate = hub.user_gate(user_id);
    let _guard = gate
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, "another chat turn is already running"))?;

    let snapshot = load_snapshot_and_claim(hub, user_id, message, request_id).await?;
    let user_msg_id = snapshot.user_msg_id;
    hub.events
        .emit(user_id.to_owned(), "message", json!({"pending": true}));

    match drive_turn(hub, user_id, message, snapshot).await {
        Ok(out) => Ok(out),
        Err(err) => {
            mark_turn_failed(hub, user_id, user_msg_id).await;
            Err(err)
        }
    }
}

/// Reads everything the brain needs for one turn (`user_msg_id` is filled in by the caller).
fn build_snapshot_sync(
    conn: &Connection,
    uid: &str,
    message: &str,
    with_history: bool,
) -> Result<TurnSnapshot> {
    let profile = read_profile_sync(conn, uid)?;
    let (rolling_summary, _) = read_compaction_state_sync(conn, uid)?;
    let threads = list_active_threads_sync(conn, uid)?;
    let ledgers = list_ledgers_sync(conn, uid)?;
    let nodes = nodes::list_nodes_sync(conn, uid)?;
    let crons = automation::list_crons_sync(conn, uid)?;
    let rules = automation::list_rules_sync(conn, uid)?;
    let matched_rule = automation::match_rule_sync(conn, uid, message)?;
    let gitlab_configured: bool = conn
        .query_row(
            "SELECT config_json FROM integrations WHERE user_id = ?1 AND kind = 'gitlab'",
            [uid],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["token"].as_str().map(|t| !t.trim().is_empty()))
        .unwrap_or(false);

    let history = if !with_history {
        Vec::new()
    } else {
        let mut hist_stmt = conn.prepare(
            "SELECT role, content, thread_id, context FROM (
                SELECT id, role, content, thread_id, context FROM messages
                WHERE user_id = ?1 AND status = 'complete' AND role IN ('user','assistant')
                ORDER BY id DESC LIMIT 10
             ) ORDER BY id ASC",
        )?;
        let raw_hist: Vec<(String, String, Option<String>, String)> = hist_stmt
            .query_map([uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let total_hist = raw_hist.len();
        raw_hist
            .into_iter()
            .enumerate()
            .map(|(idx, (role, content, thr_id, context))| {
                // Keep the newest 2 turns detailed; trim older turns tightly so long node outputs don't bloat context.
                let budget = if idx + 2 >= total_hist { 1200 } else { 480 };
                let full = if context.trim().is_empty() {
                    content
                } else {
                    format!("{content}\n{}", context.trim())
                };
                let trimmed = truncate_bytes(&full, budget);
                // Only user turns carry the thread tag: tagging assistant turns taught the model
                // to start its own replies with `[thr_xxx]`, leaking internal ids to the user.
                let tagged = match thr_id.as_deref().filter(|s| !s.is_empty()) {
                    Some(tid) if role == "user" => format!("[{tid}] {trimmed}"),
                    _ => trimmed,
                };
                ChatMessage {
                    role,
                    content: tagged,
                }
            })
            .collect::<Vec<_>>()
    };

    let contacts = {
        let mut contact_stmt = conn.prepare(
            "SELECT username, display_name FROM users WHERE status='active' AND id != ?1 ORDER BY username ASC LIMIT 30",
        )?;
        let rows = contact_stmt
            .query_map([uid], |r| {
                Ok(format!(
                    "{} ({})",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    Ok(TurnSnapshot {
        profile,
        rolling_summary,
        threads,
        ledgers,
        nodes,
        crons,
        rules,
        matched_rule,
        gitlab_configured,
        history,
        contacts,
        user_msg_id: 0,
    })
}

/// Loads the workbench snapshot and records the user message as `pending` in one transaction.
/// Retrying a failed `request_id` revives the same row instead of inserting a duplicate.
async fn load_snapshot_and_claim(
    hub: &Hub,
    user_id: &str,
    message: &str,
    request_id: Option<&str>,
) -> Result<TurnSnapshot> {
    let uid = user_id.to_owned();
    let msg_owned = message.to_owned();
    let rid_owned = request_id.map(str::to_owned);
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut snap = build_snapshot_sync(&tx, &uid, &msg_owned, true)?;
            let now = now_unix();
            let existing: Option<(i64, String)> = match rid_owned.as_deref() {
                Some(rid) => tx
                    .query_row(
                        "SELECT id, status FROM messages WHERE user_id=?1 AND request_id=?2",
                        params![uid, rid],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?,
                None => None,
            };
            let user_msg_id = match existing {
                Some((id, status)) if status == "failed" => {
                    tx.execute(
                        "UPDATE messages SET status='pending', content=?2, thread_id=NULL, created_at=?3
                         WHERE id=?1",
                        params![id, msg_owned, now],
                    )?;
                    id
                }
                Some(_) => {
                    return Err(Error(
                        StatusCode::CONFLICT,
                        "request is already being processed",
                    ))
                }
                None => {
                    tx.execute(
                        "INSERT INTO messages(user_id, role, content, request_id, status, created_at)
                         VALUES (?1, 'user', ?2, ?3, 'pending', ?4)",
                        params![uid, msg_owned, rid_owned, now],
                    )?;
                    tx.last_insert_rowid()
                }
            };
            tx.commit()?;
            snap.user_msg_id = user_msg_id;
            Ok(snap)
        })
        .await
}

async fn mark_turn_failed(hub: &Hub, user_id: &str, user_msg_id: i64) {
    let uid = user_id.to_owned();
    let _ = hub
        .db
        .write(move |conn| {
            conn.execute(
                "UPDATE messages SET status='failed' WHERE id=?1 AND user_id=?2 AND status='pending'",
                params![user_msg_id, uid],
            )?;
            Ok(())
        })
        .await;
    hub.events
        .emit(user_id.to_owned(), "stage", json!({"stage": ""}));
    hub.events
        .emit(user_id.to_owned(), "message", json!({"failed": true}));
}

/// Result of executing one tool call, reported back to the model in the next agent step.
struct ToolOutcome {
    tool: String,
    ok: bool,
    /// Factual result for the model (fed back as a TOOL_RESULTS message).
    observation: String,
    /// Text appended to the user-facing reply if the agent loop ends before the model
    /// could incorporate this outcome itself.
    user_note: String,
    /// Whether the model must see this result before the turn can be answered well
    /// (query results and failures). Successful fire-and-forget mutations don't.
    needs_followup: bool,
}

impl ToolOutcome {
    fn done(tool: &str, observation: impl Into<String>) -> Self {
        Self {
            tool: tool.to_owned(),
            ok: true,
            observation: observation.into(),
            user_note: String::new(),
            needs_followup: false,
        }
    }

    fn failed(tool: &str, observation: impl Into<String>, user_note: impl Into<String>) -> Self {
        Self {
            tool: tool.to_owned(),
            ok: false,
            observation: observation.into(),
            user_note: user_note.into(),
            needs_followup: true,
        }
    }
}

/// Mutable per-turn state shared across agent steps.
struct TurnContext<'a> {
    hub: &'a Hub,
    user_id: &'a str,
    message: &'a str,
    node_list: &'a [nodes::NodeView],
    matched_rule_target: Option<String>,
    active_thread_id: Option<String>,
    dispatched_tasks: Vec<Value>,
    /// Keys of side-effecting calls that already succeeded this turn (prevents follow-up
    /// steps from re-dispatching the same job).
    done_keys: HashSet<String>,
    /// Interactive chat turns stream `stage` progress to the UI; headless runs stay silent.
    interactive: bool,
    /// Optional tool allow-list (headless/cron runs must not e.g. create more crons).
    allowed_tools: Option<&'a [&'a str]>,
}

impl TurnContext<'_> {
    fn stage(&self, text: &str) {
        if self.interactive {
            self.hub
                .events
                .emit(self.user_id.to_owned(), "stage", json!({ "stage": text }));
        }
    }
}

/// Final result of one agent loop run.
pub struct AgentOutput {
    pub reply: String,
    pub thread_id: Option<String>,
    pub tasks: Vec<Value>,
    /// Model-only context derived from the user message (e.g. image captions), persisted so
    /// follow-up turns can still refer to it.
    pub user_context: String,
}

/// How an agent loop run is driven.
struct AgentMode<'a> {
    /// Extra system prompt section appended after the standard prompt.
    prompt_suffix: &'a str,
    interactive: bool,
    allowed_tools: Option<&'a [&'a str]>,
}

/// Runs an interactive chat turn: agent loop, then atomically persists the final reply.
async fn drive_turn(hub: &Hub, user_id: &str, message: &str, snap: TurnSnapshot) -> Result<Value> {
    let mode = AgentMode {
        prompt_suffix: "",
        interactive: true,
        allowed_tools: None,
    };
    let out = agent_loop(hub, user_id, message, &snap, &mode).await?;
    let reply = persist_turn(
        hub,
        user_id,
        message,
        snap.user_msg_id,
        out.thread_id.clone(),
        &out.reply,
        &out.user_context,
    )
    .await?;
    // Non-blocking background rolling context compaction & Active Memory pruning.
    maybe_spawn_compaction(hub.clone(), user_id.to_owned());
    Ok(json!({
        "reply": reply,
        "thread_id": out.thread_id,
        "tasks": out.tasks,
    }))
}

/// Runs the brain without a chat turn (e.g. cron jobs): no user message is stored, no
/// history is used, and only `allowed_tools` may be called. The caller persists the reply.
pub async fn run_headless(
    hub: &Hub,
    user_id: &str,
    instruction: &str,
    prompt_suffix: &str,
    allowed_tools: &[&str],
) -> Result<AgentOutput> {
    let uid = user_id.to_owned();
    let instr = instruction.to_owned();
    let snap = hub
        .db
        .read(move |conn| build_snapshot_sync(conn, &uid, &instr, false))
        .await?;
    let mode = AgentMode {
        prompt_suffix,
        interactive: false,
        allowed_tools: Some(allowed_tools),
    };
    agent_loop(hub, user_id, instruction, &snap, &mode).await
}

/// The agent loop: LLM → tools → feed results back → … (at most `MAX_AGENT_STEPS` calls).
async fn agent_loop(
    hub: &Hub,
    user_id: &str,
    message: &str,
    snap: &TurnSnapshot,
    mode: &AgentMode<'_>,
) -> Result<AgentOutput> {
    // Build structured system prompt with per-section budgets (system rules & tools are NEVER truncated).
    let mut system_prompt = build_system_prompt(
        &snap.profile,
        &snap.rolling_summary,
        &snap.threads,
        &snap.ledgers,
        &snap.nodes,
        &snap.crons,
        &snap.rules,
        &snap.contacts,
        snap.gitlab_configured,
    );
    system_prompt.push_str(mode.prompt_suffix);

    let mut ctx = TurnContext {
        hub,
        user_id,
        message,
        node_list: &snap.nodes,
        matched_rule_target: snap.matched_rule.as_ref().map(|r| r.target_node_id.clone()),
        active_thread_id: None,
        dispatched_tasks: Vec::new(),
        done_keys: HashSet::new(),
        interactive: mode.interactive,
        allowed_tools: mode.allowed_tools,
    };

    let models = crate::models::resolve(hub, user_id).await?;
    let prepared = prepare_images(&ctx, &models, message).await;

    let mut convo = snap.history.clone();
    let mut prompt_msg = if prepared.context.is_empty() {
        message.to_owned()
    } else {
        format!("{message}\n\n{}", prepared.context)
    };
    let mut step_images = prepared.inline;
    let mut final_reply = String::new();
    let mut unresolved: Vec<ToolOutcome> = Vec::new();

    for step in 0..MAX_AGENT_STEPS {
        // Inline images ride along with the first call only; later steps see the text history.
        let images = std::mem::take(&mut step_images);
        let raw_llm = match models
            .brain
            .complete_with_images(&system_prompt, &convo, &prompt_msg, &images, true)
            .await
        {
            Ok(raw) => raw,
            // Without a first reply there is nothing to show: the turn fails.
            Err(err) if step == 0 => return Err(err),
            // A failed follow-up keeps the previous reply plus the unresolved tool notes.
            Err(_) => break,
        };
        let parsed = parse_brain_response(&raw_llm);
        final_reply = parsed.reply;

        let resolved = resolve_or_create_thread(
            hub,
            user_id,
            parsed.thread_id.as_deref(),
            parsed.thread_title.as_deref(),
            message,
        )
        .await?;
        if step == 0 || resolved.is_some() {
            ctx.active_thread_id = resolved;
        }

        let mut outcomes = Vec::with_capacity(parsed.tools.len());
        for call in &parsed.tools {
            outcomes.push(execute_tool(&mut ctx, call).await);
        }
        if !outcomes.iter().any(|o| o.needs_followup) {
            unresolved.clear();
            break;
        }
        let last_step = step + 1 == MAX_AGENT_STEPS;
        if !last_step {
            ctx.stage("正在根据工具结果整理回复…");
            convo.push(ChatMessage {
                role: "user".into(),
                content: truncate_bytes(&prompt_msg, 8000),
            });
            convo.push(ChatMessage {
                role: "assistant".into(),
                content: truncate_bytes(raw_llm.trim(), 4000),
            });
            prompt_msg = format_tool_results(&outcomes);
        }
        unresolved = outcomes;
    }

    for o in unresolved.iter().filter(|o| o.needs_followup) {
        if !o.user_note.trim().is_empty() {
            final_reply.push_str("\n\n");
            final_reply.push_str(o.user_note.trim());
        }
    }
    final_reply = strip_thread_tags(&final_reply);
    if final_reply.trim().is_empty() {
        final_reply = "好的，已处理。".into();
    }

    Ok(AgentOutput {
        reply: final_reply,
        thread_id: ctx.active_thread_id,
        tasks: ctx.dispatched_tasks,
        user_context: prepared.persist_context,
    })
}

/// Atomically completes the user message and stores the assistant reply linked via `reply_to`.
async fn persist_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    user_msg_id: i64,
    thread_id: Option<String>,
    final_reply: &str,
    user_context: &str,
) -> Result<String> {
    let uid = user_id.to_owned();
    let context_db = truncate_bytes(user_context.trim(), 6000);
    let reply_db = truncate_bytes(final_reply.trim(), 16_000);
    let reply_ret = reply_db.clone();
    let thr_summary_snippet = truncate_bytes(
        &format!(
            "{} -> {}",
            truncate_bytes(message, 80),
            truncate_bytes(&reply_db, 120)
        ),
        220,
    );
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = now_unix();
            let claimed = tx.execute(
                "UPDATE messages SET status = 'complete', thread_id = ?3, context = ?4
                 WHERE id = ?1 AND user_id = ?2 AND status = 'pending'",
                params![user_msg_id, uid, thread_id, context_db],
            )?;
            if claimed != 1 {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "chat turn was superseded",
                ));
            }
            if let Some(ref tid) = thread_id {
                tx.execute(
                    "UPDATE threads
                     SET turn_count = turn_count + 1,
                         summary = CASE WHEN summary = '' THEN ?3 ELSE summary END,
                         updated_at = ?4
                     WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid, thr_summary_snippet, now],
                )?;
            }
            tx.execute(
                "INSERT INTO messages(user_id, thread_id, role, content, status, reply_to, created_at)
                 VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4, ?5)",
                params![uid, thread_id, reply_db, user_msg_id, now],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;

    hub.events
        .emit(user_id.to_owned(), "stage", json!({"stage": ""}));
    hub.events
        .emit(user_id.to_owned(), "message", json!({"done": true}));
    Ok(reply_ret)
}

fn format_tool_results(outcomes: &[ToolOutcome]) -> String {
    let mut out = format!(
        "{}\n以下是你上一步调用工具的执行结果（由系统自动回传，不是用户发言）：\n",
        crate::llm::TOOL_RESULTS_MARKER
    );
    for o in outcomes {
        out.push_str(&format!(
            "- [{}] {}：{}\n",
            o.tool,
            if o.ok { "成功" } else { "失败" },
            truncate_bytes(o.observation.trim(), TOOL_OBSERVATION_MAX_BYTES)
        ));
    }
    out.push_str(
        "\n请基于以上结果给出面向用户的最终完整回复（会替换上一步的 reply）。\
         如确有必要可继续调用工具，但不要重复已成功的调用。仍严格按同样的 JSON 格式输出。",
    );
    out
}

/// Identity of a side-effecting call, used to skip exact repeats within one turn.
fn side_effect_key(call: &ToolCall) -> Option<String> {
    let non_idempotent = matches!(
        call.tool.as_str(),
        "dispatch_node" | "delegate_colleague" | "manage_cron" | "manage_rule"
    );
    non_idempotent.then(|| {
        [
            call.tool.as_str(),
            call.action.as_deref().unwrap_or(""),
            call.peer.as_deref().unwrap_or(""),
            call.keyword.as_deref().unwrap_or(""),
            call.instruction.as_deref().unwrap_or("").trim(),
        ]
        .join("\u{1f}")
    })
}

async fn execute_tool(ctx: &mut TurnContext<'_>, call: &ToolCall) -> ToolOutcome {
    if ctx
        .allowed_tools
        .is_some_and(|allowed| !allowed.contains(&call.tool.as_str()))
    {
        return ToolOutcome::failed(
            &call.tool,
            format!(
                "当前运行模式不允许调用 `{}`，请改用其它方式完成。",
                call.tool
            ),
            String::new(),
        );
    }
    let key = side_effect_key(call);
    if key.as_ref().is_some_and(|k| ctx.done_keys.contains(k)) {
        return ToolOutcome::done(&call.tool, "本轮已执行过相同调用，已跳过以避免重复。");
    }
    let outcome = match run_tool(ctx, call).await {
        Ok(o) => o,
        Err(err) => ToolOutcome::failed(
            &call.tool,
            format!("执行出错：{}", err.1),
            format!("（{} 执行失败：{}）", call.tool, err.1),
        ),
    };
    if outcome.ok {
        if let Some(k) = key {
            ctx.done_keys.insert(k);
        }
    }
    outcome
}

async fn run_tool(ctx: &mut TurnContext<'_>, call: &ToolCall) -> Result<ToolOutcome> {
    let hub = ctx.hub;
    let user_id = ctx.user_id;
    let tool = call.tool.as_str();
    match tool {
        "update_memory" => {
            apply_update_memory(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新人设/长期记忆。"))
        }
        "update_ledger" => {
            apply_update_ledger(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新持续事项。"))
        }
        "close_thread" => {
            let target = call
                .thread_id
                .as_deref()
                .or(ctx.active_thread_id.as_deref())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            match target {
                Some(tid) => {
                    archive_thread(hub, user_id, &tid).await?;
                    Ok(ToolOutcome::done(tool, format!("已归档线程 {tid}。")))
                }
                None => Ok(ToolOutcome::done(tool, "没有可归档的线程，已忽略。")),
            }
        }
        "manage_cron" => {
            apply_manage_cron(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新定时任务。"))
        }
        "manage_rule" => {
            apply_manage_rule(hub, user_id, call).await?;
            Ok(ToolOutcome::done(tool, "已更新分发规则。"))
        }
        "fetch_gitlab_commits" => {
            ctx.stage("正在拉取 GitLab 提交记录…");
            if let Some(summary) =
                crate::integrations::gitlab::fetch_commit_summary(hub, user_id).await
            {
                return Ok(ToolOutcome {
                    tool: tool.to_owned(),
                    ok: true,
                    observation: summary.clone(),
                    user_note: summary,
                    needs_followup: true,
                });
            }
            let online_pc: Vec<String> = ctx
                .node_list
                .iter()
                .filter(|n| n.online && n.kind == "pc")
                .map(|n| format!("{}（{}）", n.id, n.name))
                .collect();
            let observation = if online_pc.is_empty() {
                "未能拉取 GitLab 提交：未配置 GitLab 访问令牌或请求失败；当前也没有在线的本机节点可查询本地仓库。".to_owned()
            } else {
                format!(
                    "未能拉取 GitLab 提交：未配置 GitLab 访问令牌或请求失败。在线本机节点：{}。\
                     如用户需要提交记录，可调用 dispatch_node（prefer_cloud=false）让节点在本地仓库运行 git log 并汇总。",
                    online_pc.join("、")
                )
            };
            Ok(ToolOutcome::failed(
                tool,
                observation,
                "（未能拉取 GitLab 提交记录：尚未配置 GitLab 访问令牌或请求失败。）",
            ))
        }
        "dispatch_node" => {
            ctx.stage("正在派发到执行节点…");
            let mut instr = call
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(ctx.message)
                .to_owned();
            // Nodes only receive attachments referenced in the instruction; the model often
            // paraphrases them away, so forward the user's attachments explicitly.
            let missing: Vec<String> = crate::attachments::extract_attachment_ids(ctx.message)
                .into_iter()
                .filter(|id| !instr.contains(id.as_str()))
                .collect();
            if !missing.is_empty() {
                instr.push_str("\n\n用户附件：");
                for id in missing {
                    instr.push_str(&format!("\n![附件](/api/attachments/{id})"));
                }
            }
            let title = call
                .title
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| instr.lines().next().unwrap_or("节点任务"))
                .to_owned();
            let preferred_node = call
                .node_id
                .clone()
                .or_else(|| ctx.matched_rule_target.clone());
            let prefer_cloud = call.prefer_cloud.unwrap_or(false);
            // Ensure every node dispatch belongs to an implicit backend thread for future session resume.
            let task_thread_id = match resolve_or_create_thread(
                hub,
                user_id,
                call.thread_id
                    .as_deref()
                    .or(ctx.active_thread_id.as_deref()),
                Some(&title),
                &title,
            )
            .await?
            {
                Some(tid) => tid,
                None => create_thread(hub, user_id, &title).await?,
            };
            ctx.active_thread_id = Some(task_thread_id.clone());
            match dispatch_to_node(
                hub,
                user_id,
                preferred_node.as_deref(),
                prefer_cloud,
                &title,
                &instr,
                Some(&task_thread_id),
            )
            .await?
            {
                Some(info) => {
                    let observation = format!(
                        "已派发任务 {} 到节点 {}（{}，{}），节点完成后结果会自动发回聊天。",
                        info["task_id"].as_str().unwrap_or(""),
                        info["node_name"].as_str().unwrap_or(""),
                        info["node_kind"].as_str().unwrap_or(""),
                        if info["online"].as_bool().unwrap_or(false) {
                            "在线"
                        } else {
                            "离线，上线后执行"
                        }
                    );
                    ctx.dispatched_tasks.push(info);
                    Ok(ToolOutcome::done(tool, observation))
                }
                None => Ok(ToolOutcome::failed(
                    tool,
                    "派发失败：当前没有已注册的 PC 或云端执行节点。",
                    "（当前没有已注册的 PC 或云端执行节点，请先在设置中接入节点。）",
                )),
            }
        }
        "delegate_colleague" => {
            let Some(peer) = call
                .peer
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return Ok(ToolOutcome::failed(
                    tool,
                    "缺少 peer 参数。",
                    "（协作请求缺少同事名称。）",
                ));
            };
            let instr = call
                .instruction
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(ctx.message);
            match collab::delegate_to_peer(hub, user_id, peer, instr).await {
                Ok((peer_name, task_id, mode)) => {
                    ctx.dispatched_tasks.push(json!({
                        "task_id": task_id,
                        "peer": peer_name,
                        "mode": mode,
                    }));
                    Ok(ToolOutcome::done(
                        tool,
                        format!("已向 {peer_name} 发起协作任务 {task_id}（模式：{mode}）。"),
                    ))
                }
                Err(_) => Ok(ToolOutcome::failed(
                    tool,
                    format!("未找到名为 `{peer}` 的活跃同事。"),
                    format!("（未找到名为 `{peer}` 的活跃同事。）"),
                )),
            }
        }
        other => Ok(ToolOutcome::failed(
            other,
            format!("未知工具 `{other}`，请只使用系统提示中列出的工具。"),
            String::new(),
        )),
    }
}

const VISION_CAPTION_PROMPT: &str =
    "VISION_CAPTION_TASK：你是识图助手。用中文客观描述这张图片，供另一个只能读文字的助手理解：\
先一句话说明图片类型（终端/报错截图、网页或应用界面、文档、照片、图表等），\
再逐字转录图中所有关键文字（报错信息、命令、日志、标题、按钮、表格数据），最后补充布局或异常之处。\
不要猜测图片之外的信息，不要给建议。";

/// How the images in a user message reach the brain.
struct PreparedImages {
    /// Images sent inline to an image-capable brain.
    inline: Vec<crate::llm::ImageInput>,
    /// Extra text appended to the prompt (captions or an honest "can't see images" note).
    context: String,
    /// Part of `context` worth persisting for follow-up turns (captions only).
    persist_context: String,
}

async fn prepare_images(
    ctx: &TurnContext<'_>,
    models: &crate::models::ResolvedModels,
    message: &str,
) -> PreparedImages {
    let mut out = PreparedImages {
        inline: Vec::new(),
        context: String::new(),
        persist_context: String::new(),
    };
    let atts = crate::attachments::collect_for_user(ctx.hub, ctx.user_id, message)
        .await
        .unwrap_or_default();
    let images: Vec<(String, crate::llm::ImageInput)> = atts
        .into_iter()
        .filter(|a| {
            a["mime_type"]
                .as_str()
                .is_some_and(|m| m.starts_with("image/"))
        })
        .filter_map(|a| {
            Some((
                a["name"].as_str()?.to_owned(),
                crate::llm::ImageInput {
                    mime_type: a["mime_type"].as_str()?.to_owned(),
                    data_base64: a["data_base64"].as_str()?.to_owned(),
                },
            ))
        })
        .collect();
    if images.is_empty() {
        return out;
    }
    if models.brain_sees_images {
        out.inline = images.into_iter().map(|(_, img)| img).collect();
        return out;
    }
    let Some(vision) = &models.vision else {
        out.context = format!(
            "（系统提示：用户附带了 {} 张图片，但当前大脑模型不能看图，也没有配置识图模型，你看不到图片内容。\
             请如实告诉用户：可在「设置 → 大脑模型」里换用支持图片的大脑模型，或配置一个识图模型；\
             也可以先把图中关键文字贴成文本。）",
            images.len()
        );
        return out;
    };
    ctx.stage("正在识别图片…");
    let mut captions = Vec::new();
    for (name, img) in &images {
        let caption = match vision
            .complete_with_images(
                VISION_CAPTION_PROMPT,
                &[],
                "请描述这张图片。",
                std::slice::from_ref(img),
                false,
            )
            .await
        {
            Ok(text) => truncate_bytes(text.trim(), 2500),
            Err(e) => format!("（识别失败：{}）", e.1),
        };
        captions.push(format!("【图片 {name} 内容（由识图模型生成）】\n{caption}"));
    }
    out.context = captions.join("\n\n");
    out.persist_context = out.context.clone();
    out
}

/// Removes internal `[thr_xxx]` thread tags the model may echo at the start of lines.
fn strip_thread_tags(reply: &str) -> String {
    reply
        .lines()
        .map(|line| {
            let mut rest = line;
            loop {
                let trimmed = rest.trim_start();
                let Some(after) = trimmed.strip_prefix("[thr_") else {
                    break;
                };
                let Some(end) = after.find(']') else { break };
                if end == 0 || !after[..end].bytes().all(|b| b.is_ascii_alphanumeric()) {
                    break;
                }
                rest = after[end + 1..].trim_start();
            }
            rest
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

#[allow(clippy::too_many_arguments)]
fn build_system_prompt(
    profile: &AgentProfile,
    rolling_summary: &str,
    threads: &[ThreadView],
    ledgers: &[Ledger],
    node_list: &[nodes::NodeView],
    crons: &[automation::CronJob],
    rules: &[automation::DispatchRule],
    contacts: &[String],
    gitlab_configured: bool,
) -> String {
    let soul = truncate_bytes(&profile.soul_md, 1500);
    let memory = truncate_bytes(&profile.memory_md, 2000);
    let summary_section = if rolling_summary.trim().is_empty() {
        "（暂无早期历史摘要）".into()
    } else {
        truncate_bytes(rolling_summary.trim(), 2000)
    };

    let threads_str = if threads.is_empty() {
        "（无近期活跃线程）".into()
    } else {
        truncate_bytes(
            &threads
                .iter()
                .map(|t| {
                    let node_info = match (&t.node_id, t.has_session) {
                        (Some(nid), true) => format!(" | 节点={nid}(可续接session)"),
                        (Some(nid), false) => format!(" | 节点={nid}"),
                        _ => String::new(),
                    };
                    format!(
                        "- id=`{}` | 「{}」{} | {}",
                        t.id,
                        t.title,
                        node_info,
                        truncate_bytes(&t.summary, 180)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            1400,
        )
    };

    let ledgers_str = if ledgers.is_empty() {
        "（无）".into()
    } else {
        truncate_bytes(
            &ledgers
                .iter()
                .take(15)
                .map(|l| {
                    format!(
                        "- id=`{}` | 「{}」 | 状态={} | {}",
                        l.id,
                        l.title,
                        l.status,
                        truncate_bytes(&l.notes, 240)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            2000,
        )
    };

    let nodes_str = if node_list.is_empty() {
        "（无已注册节点）".into()
    } else {
        node_list
            .iter()
            .map(|n| {
                format!(
                    "- id=`{}` | 名称=「{}」 | 类型={} | {} | {}",
                    n.id,
                    n.name,
                    n.kind,
                    if n.online { "在线" } else { "离线" },
                    truncate_bytes(&n.capabilities, 240)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let crons_str = if crons.is_empty() {
        "（无）".into()
    } else {
        crons
            .iter()
            .take(10)
            .map(|c| {
                format!(
                    "- id=`{}` | {} | target={} | {}",
                    c.id,
                    c.daily_time_cst
                        .as_deref()
                        .map(|t| format!("每天 {t}"))
                        .unwrap_or_else(|| format!("每 {} 分钟", c.interval_minutes)),
                    c.target,
                    truncate_bytes(&c.instruction, 100)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let rules_str = if rules.is_empty() {
        "（无）".into()
    } else {
        rules
            .iter()
            .take(10)
            .map(|r| {
                format!(
                    "- id=`{}` | 关键词=「{}」 -> {}",
                    r.id, r.keyword, r.target_node_id
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let contacts_str = if contacts.is_empty() {
        "（无其他同事）".into()
    } else {
        contacts.join(", ")
    };

    let gitlab_hint = if gitlab_configured {
        "GitLab PAT 已配置（仅用于查全局推送事件；查特定本地仓库提交可按需用 dispatch_node）"
    } else {
        "GitLab PAT 未配置（查任意代码仓库的 git 提交/分支/代码，请直接用 dispatch_node 派给在线本机节点，勿调 fetch_gitlab_commits）"
    };

    format!(
        r#"你是用户的 7×24h 个人超级智能体「{name}」。请始终用第一人称、干练、结论先行的中文交流。

=== 自动语义归类、防串台与必要时反问（核心纪律） ===
- 用户只在单一聊天框里自然说话，不手动选线程；由你判断这句话在承接哪个隐式线程（`thread_id`）、还是开启新主题（`"new"`）、还是普通闲聊（`null`）。
- 近期对话中用户消息前的 `[thr_xxx]` 标注了该轮所属的隐式线程（仅供你内部判断，回复中绝不要输出这类标记），下方【隐式上下文线程】列出了活跃线程和是否可续接节点 session。
- **指代清晰时直接承接**：如语义明确对应某个活跃线程，直接填该 `thread_id`；若调用 `dispatch_node` 继续排查或修改，必须带上同一 `thread_id`（系统会自动续接该节点上次的 `opencode --session`）。
- **真正存在歧义时必须先简短反问，禁止盲猜执行**：例如同时有 2 个以上近期活跃线程/节点回执，用户只说「帮我改一下」「继续查」「把刚才那个关了」且无法从语义唯一判断指向哪一个时，**禁止调用 `dispatch_node` 或变更类工具**，直接在 `reply` 中用一句话自然反问确认（例如「你是指刚才的 A 还是 B？」），并设 `thread_id` 为 `null`。
- 明确完结的临时话题可顺手调用 `close_thread` 归档，保持线程列表清爽。

=== 自动维护记忆与持续事项 ===
- 用户不需要手工填表维护人设、记忆、持续事项或定时任务；你负责在对话中自动维护。
- 只要用户在对话中表达称呼偏好（如「以后叫你…」）、沟通风格要求、纠偏规矩、长期工作事实或明确说「记住…」，当轮必须顺手调用 `update_memory` 落盘，并在回复末尾用一句简短自然的话交代（如「已记住」）。
- 只要用户跟进某个持续推进的事项、提出新进展/待办或要求某件事别再提醒（静默挂起），当轮顺手调用 `update_ledger` 维护对应持续事项。
- 只要用户用自然语言新增、修改或取消定时任务，当轮直接调用 `manage_cron` 生效。

=== 当前人设风格 (SOUL) ===
{soul}

=== 当前长期记忆 (MEMORY) ===
{memory}

=== 早期历史对话压缩摘要 ===
{summary_section}

=== 实时工作台快照 ===
【隐式上下文线程 (threads，仅后台语义与节点 session 隔离，勿向用户暴露 thread_id)】
{threads_str}

【持续事项 (ledgers)】
{ledgers_str}

【执行节点 (nodes: pc=个人电脑, cloud=云端常驻容器)】
{nodes_str}

【定时任务 (crons)】
{crons_str}

【分发规则 (rules)】
{rules_str}

【活跃同事 (contacts)】
{contacts_str}

【GitLab 集成状态】
{gitlab_hint}

=== 输出格式（严格合法 JSON，勿加 Markdown 代码块，严禁输出 <tool_call> / </tool_call> 标签或裸工具 JSON） ===
{{"reply":"直接展示给用户的自然中文回复（必填）","thread_id":"thr_xxx 或 new 或 null","thread_title":"仅当 thread_id=new 时给简短标题，否则 null","tools":[]}}

=== 工具结果回传 ===
- 查询类工具（如 fetch_gitlab_commits）的结果、以及任何工具执行失败，会由系统以 `TOOL_RESULTS` 开头的消息回传给你；届时请基于真实结果给出最终完整回复（替换上一步的 reply），不要编造工具没有返回的内容。
- 调用查询类工具的那一步 reply 只需一句简短过渡语。

=== 可组合调用的工具（必须放在 tools 数组中，严禁单独输出） ===
1. update_memory: {{"tool":"update_memory","name":null,"avatar_id":"agenct|sakura|violet|cheshire或null","soul_md":null,"memory_append":"追加一条长期记忆/规矩","memory_replace":null}}
2. update_ledger: {{"tool":"update_ledger","action":"upsert|mute|done|delete","ledger_id":null,"title":"专项名","status":"active|muted|done","notes_append":"增量记录","notes_replace":null}}
3. manage_cron: {{"tool":"manage_cron","action":"add|delete","cron_id":null,"daily_time_cst":"HH:MM或null","interval_minutes":60,"target":"cloud|auto|node_id","instruction":"完整指令"}}
4. manage_rule: {{"tool":"manage_rule","action":"add|delete","rule_id":null,"keyword":"关键词","node_id":"目标节点或cloud"}}
5. fetch_gitlab_commits: {{"tool":"fetch_gitlab_commits"}}（仅在 GitLab PAT 已配置时使用）
6. dispatch_node: {{"tool":"dispatch_node","thread_id":"沿用的 thr_xxx 或 null","node_id":null,"prefer_cloud":false,"title":"简短标题","instruction":"补全全部上下文指代后的自包含执行指令"}}
   - 涉及本机代码/git 提交/tmux/私有环境时 prefer_cloud=false；通用云端脚本/分析/爬虫/截图任务可设 prefer_cloud=true。
7. close_thread: {{"tool":"close_thread","thread_id":"已完结的 thr_xxx"}}
8. delegate_colleague: {{"tool":"delegate_colleague","peer":"同事username或姓名","instruction":"需要同事协助的清晰事项"}}"#,
        name = profile.name,
    )
}

fn default_reply_for_tools(tools: &[ToolCall]) -> String {
    if tools.iter().any(|t| t.tool == "dispatch_node") {
        return "已派发给执行节点处理，完成后我会把结果发回来。".into();
    }
    if tools.iter().any(|t| t.tool == "fetch_gitlab_commits") {
        return "好的，我来查一下 GitLab 提交记录。".into();
    }
    if tools.iter().any(|t| t.tool == "delegate_colleague") {
        return "已发起协作请求。".into();
    }
    if tools.iter().any(|t| t.tool == "manage_cron") {
        return "已更新定时任务。".into();
    }
    if tools
        .iter()
        .any(|t| matches!(t.tool.as_str(), "update_memory" | "update_ledger"))
    {
        return "好的，已记录。".into();
    }
    "好的，已处理。".into()
}

fn strip_hallucinated_tags(raw: &str) -> String {
    let mut s = raw.trim().to_owned();
    // Strip <think>...</think> blocks if any reasoning model emits them.
    while let (Some(a), Some(b)) = (s.find("<think>"), s.find("</think>")) {
        if b > a {
            s.replace_range(a..b + "</think>".len(), "");
        } else {
            break;
        }
    }
    for tag in [
        "<tool_call>",
        "</tool_call>",
        "<function_call>",
        "</function_call>",
    ] {
        s = s.replace(tag, "");
    }
    s.trim().to_owned()
}

fn parse_brain_response(raw: &str) -> BrainResponse {
    let cleaned = strip_hallucinated_tags(raw);
    if let (Some(s), Some(e)) = (cleaned.find('{'), cleaned.rfind('}')) {
        let json_slice = &cleaned[s..=e];
        if let Ok(mut parsed) = serde_json::from_str::<BrainResponse>(json_slice) {
            if !parsed.reply.trim().is_empty() || !parsed.tools.is_empty() {
                if parsed.reply.trim().is_empty() {
                    parsed.reply = default_reply_for_tools(&parsed.tools);
                }
                return parsed;
            }
        }
        // Fallback 1: model emitted a bare tool object like `{"tool":"fetch_gitlab_commits"}`.
        if let Ok(single_tool) = serde_json::from_str::<ToolCall>(json_slice) {
            if !single_tool.tool.trim().is_empty() {
                let prose = format!("{} {}", cleaned[..s].trim(), cleaned[e + 1..].trim())
                    .trim()
                    .to_owned();
                let tools = vec![single_tool];
                let reply = if prose.is_empty() {
                    default_reply_for_tools(&tools)
                } else {
                    prose
                };
                return BrainResponse {
                    reply,
                    thread_id: None,
                    thread_title: None,
                    tools,
                };
            }
        }
    }
    // Fallback 2: model emitted a bare JSON array of ToolCalls `[{"tool":"..."}]`.
    if let (Some(s), Some(e)) = (cleaned.find('['), cleaned.rfind(']')) {
        if let Ok(tools) = serde_json::from_str::<Vec<ToolCall>>(&cleaned[s..=e]) {
            if !tools.is_empty() && tools.iter().all(|t| !t.tool.trim().is_empty()) {
                let prose = format!("{} {}", cleaned[..s].trim(), cleaned[e + 1..].trim())
                    .trim()
                    .to_owned();
                let reply = if prose.is_empty() {
                    default_reply_for_tools(&tools)
                } else {
                    prose
                };
                return BrainResponse {
                    reply,
                    thread_id: None,
                    thread_title: None,
                    tools,
                };
            }
        }
    }
    BrainResponse {
        reply: cleaned,
        thread_id: None,
        thread_title: None,
        tools: Vec::new(),
    }
}

async fn apply_update_memory(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let name_opt = call.name.clone();
    let avatar_opt = call.avatar_id.clone();
    let soul_opt = call.soul_md.clone();
    let mem_rep = call.memory_replace.clone();
    let mem_app = call.memory_append.clone();
    hub.db
        .write(move |conn| {
            let mut p = read_profile_sync(conn, &uid)?;
            if let Some(n) = name_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.name = truncate_bytes(n, 48);
            }
            if let Some(a) = avatar_opt.as_deref().and_then(normalize_avatar_id) {
                p.avatar_id = a.to_owned();
            }
            if let Some(s) = soul_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.soul_md = truncate_bytes(s, 6000);
            }
            if let Some(r) = mem_rep.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.memory_md = truncate_bytes(r, 8000);
            } else if let Some(a) = mem_app.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let line = if a.starts_with('-') {
                    a.to_owned()
                } else {
                    format!("- {a}")
                };
                if p.memory_md.trim() == DEFAULT_MEMORY_MD
                    || p.memory_md.contains("在这里记录长期偏好")
                {
                    p.memory_md = truncate_bytes(&line, 8000);
                } else if !p.memory_md.contains(&line) {
                    p.memory_md =
                        truncate_bytes(&format!("{}\n{line}", p.memory_md.trim_end()), 8000);
                }
            }
            p.updated_at = now_unix();
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, avatar_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(user_id) DO UPDATE SET
                   name=excluded.name, soul_md=excluded.soul_md,
                   memory_md=excluded.memory_md, avatar_id=excluded.avatar_id,
                   updated_at=excluded.updated_at",
                params![uid, p.name, p.soul_md, p.memory_md, p.avatar_id, p.updated_at],
            )?;
            record_audit(conn, &uid, "update_memory", "ok", "Updated SOUL/MEMORY", "")?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user_id.to_owned(), "brain", json!({"updated": "profile"}));
    Ok(())
}

async fn apply_update_ledger(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("upsert").trim().to_owned();
    let lid = call.ledger_id.clone();
    let title = call.title.clone();
    let status_in = call.status.clone();
    let rep = call.notes_replace.clone();
    let app = call.notes_append.clone();
    hub.db
        .write(move |conn| {
            let now = now_unix();
            if act == "delete" {
                if let Some(id) = lid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM ledgers WHERE id=?1 AND user_id=?2", params![id, uid])?;
                } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM ledgers WHERE title=?1 AND user_id=?2", params![t, uid])?;
                }
                return Ok(());
            }
            let next_status = match act.as_str() {
                "mute" => Some("muted"),
                "done" => Some("done"),
                _ => status_in
                    .as_deref()
                    .filter(|s| matches!(*s, "active" | "muted" | "done")),
            };
            let existing: Option<Ledger> = if let Some(id) =
                lid.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                conn.query_row(
                    "SELECT id, title, status, notes, updated_at FROM ledgers WHERE id=?1 AND user_id=?2",
                    params![id, uid],
                    |r| Ok(Ledger { id: r.get(0)?, title: r.get(1)?, status: r.get(2)?, notes: r.get(3)?, updated_at: r.get(4)? }),
                )
                .optional()?
            } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                conn.query_row(
                    "SELECT id, title, status, notes, updated_at FROM ledgers WHERE lower(title)=lower(?1) AND user_id=?2 LIMIT 1",
                    params![t, uid],
                    |r| Ok(Ledger { id: r.get(0)?, title: r.get(1)?, status: r.get(2)?, notes: r.get(3)?, updated_at: r.get(4)? }),
                )
                .optional()?
            } else {
                None
            };

            if let Some(mut cur) = existing {
                if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cur.title = truncate_bytes(t, 96);
                }
                if let Some(st) = next_status {
                    cur.status = st.to_owned();
                }
                if let Some(r) = rep.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    cur.notes = truncate_bytes(r, 6000);
                } else if let Some(a) = app.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    let line = if a.starts_with('-') { a.to_owned() } else { format!("- {a}") };
                    if !cur.notes.contains(&line) {
                        cur.notes = truncate_bytes(
                            &if cur.notes.trim().is_empty() {
                                line
                            } else {
                                format!("{}\n{line}", cur.notes.trim_end())
                            },
                            6000,
                        );
                    }
                }
                conn.execute(
                    "UPDATE ledgers SET title=?3, status=?4, notes=?5, updated_at=?6 WHERE id=?1 AND user_id=?2",
                    params![cur.id, uid, cur.title, cur.status, cur.notes, now],
                )?;
            } else if let Some(t) = title.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("ldg_{}", &Uuid::new_v4().simple().to_string()[..8]);
                let notes = rep
                    .as_deref()
                    .or(app.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| truncate_bytes(s, 6000))
                    .unwrap_or_default();
                conn.execute(
                    "INSERT INTO ledgers(id, user_id, title, status, notes, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![id, uid, truncate_bytes(t, 96), next_status.unwrap_or("active"), notes, now],
                )?;
            }
            record_audit(conn, &uid, "update_ledger", "ok", title.as_deref().unwrap_or("ledger"), "")?;
            Ok(())
        })
        .await?;
    hub.events
        .emit(user_id.to_owned(), "brain", json!({"updated": "ledgers"}));
    Ok(())
}

async fn apply_manage_cron(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("add").trim().to_owned();
    let cid = call.cron_id.clone();
    let daily = call
        .daily_time_cst
        .as_deref()
        .and_then(automation::parse_hhmm);
    let interval = if daily.is_some() {
        1440
    } else {
        call.interval_minutes.unwrap_or(60).clamp(1, 10_080)
    };
    let instr = call.instruction.clone();
    let target = call.target.clone().unwrap_or_else(|| "cloud".into());

    hub.db
        .write(move |conn| {
            if act == "delete" {
                if let Some(id) = cid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM cron_jobs WHERE id=?1 AND user_id=?2", params![id, uid])?;
                }
            } else if let Some(text) = instr.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("crn_{}", &Uuid::new_v4().simple().to_string()[..8]);
                conn.execute(
                    "INSERT INTO cron_jobs(id, user_id, interval_minutes, daily_time_cst, instruction, target, enabled, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7)",
                    params![id, uid, interval, daily, truncate_bytes(text, 1000), truncate_bytes(&target, 48), now_unix()],
                )?;
            }
            Ok(())
        })
        .await?;
    hub.scheduler_notify.notify_one();
    hub.events.emit(
        user_id.to_owned(),
        "automation",
        json!({"updated": "crons"}),
    );
    Ok(())
}

async fn apply_manage_rule(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let act = call.action.as_deref().unwrap_or("add").trim().to_owned();
    let rid = call.rule_id.clone();
    let kw = call.keyword.clone();
    let target = call.node_id.clone().unwrap_or_else(|| "cloud".into());
    hub.db
        .write(move |conn| {
            if act == "delete" {
                if let Some(id) = rid.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                    conn.execute("DELETE FROM automation_rules WHERE id=?1 AND user_id=?2", params![id, uid])?;
                }
            } else if let Some(k) = kw.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                let id = format!("rul_{}", &Uuid::new_v4().simple().to_string()[..8]);
                conn.execute(
                    "INSERT INTO automation_rules(id, user_id, keyword, target_node_id, require_human, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, ?5)",
                    params![id, uid, truncate_bytes(k, 64), truncate_bytes(&target, 48), now_unix()],
                )?;
            }
            Ok(())
        })
        .await?;
    hub.events.emit(
        user_id.to_owned(),
        "automation",
        json!({"updated": "rules"}),
    );
    Ok(())
}

async fn create_thread(hub: &Hub, user_id: &str, title: &str) -> Result<String> {
    let uid = user_id.to_owned();
    let clean_title = truncate_bytes(title.lines().next().unwrap_or("新话题").trim(), 80);
    let final_title = if clean_title.is_empty() {
        "新话题".to_owned()
    } else {
        clean_title
    };
    let tid = format!("thr_{}", &Uuid::new_v4().simple().to_string()[..8]);
    let tid_db = tid.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO threads(id, user_id, title, status, summary, turn_count, updated_at)
                 VALUES (?1, ?2, ?3, 'active', '', 0, ?4)",
                params![tid_db, uid, final_title, now_unix()],
            )?;
            Ok(())
        })
        .await?;
    Ok(tid)
}

async fn resolve_or_create_thread(
    hub: &Hub,
    user_id: &str,
    raw_thread_id: Option<&str>,
    thread_title: Option<&str>,
    fallback_title: &str,
) -> Result<Option<String>> {
    let Some(candidate) = raw_thread_id
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
    else {
        return Ok(None);
    };
    if candidate.eq_ignore_ascii_case("new") {
        let title = thread_title
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(fallback_title);
        return Ok(Some(create_thread(hub, user_id, title).await?));
    }
    let uid = user_id.to_owned();
    let cid = candidate.to_owned();
    let found: Option<String> = hub
        .db
        .write(move |conn| {
            let exists: Option<String> = conn
                .query_row(
                    "SELECT id FROM threads WHERE id = ?1 AND user_id = ?2",
                    params![cid, uid],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(ref id) = exists {
                conn.execute(
                    "UPDATE threads SET status = 'active', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                    params![id, uid, now_unix()],
                )?;
            }
            Ok(exists)
        })
        .await?;
    Ok(found)
}

async fn archive_thread(hub: &Hub, user_id: &str, thread_id: &str) -> Result<()> {
    let uid = user_id.to_owned();
    let tid = thread_id.to_owned();
    hub.db
        .write(move |conn| {
            conn.execute(
                "UPDATE threads SET status = 'archived', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                params![tid, uid, now_unix()],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

async fn dispatch_to_node(
    hub: &Hub,
    user_id: &str,
    preferred_node: Option<&str>,
    prefer_cloud: bool,
    title: &str,
    instruction: &str,
    thread_id: Option<&str>,
) -> Result<Option<Value>> {
    let uid = user_id.to_owned();
    let pref = preferred_node.map(str::to_owned);
    let t = title.to_owned();
    let instr = instruction.to_owned();
    let thr = thread_id.map(str::to_owned);
    let res = hub
        .db
        .write(move |conn| {
            let Some(node) = nodes::pick_node_sync(conn, &uid, pref.as_deref(), prefer_cloud)?
            else {
                return Ok(None);
            };
            let (task_id, _) = nodes::create_task_and_enqueue_in_thread_sync(
                conn,
                &uid,
                &uid,
                &node.id,
                &t,
                &instr,
                "manual",
                thr.as_deref(),
            )?;
            if let Some(ref tid) = thr {
                conn.execute(
                    "UPDATE threads SET node_id = COALESCE(node_id, ?3), updated_at = ?4
                     WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid, node.id, now_unix()],
                )?;
            }
            record_audit(
                conn,
                &uid,
                "dispatch_node",
                "queued",
                &format!("{} -> {}", node.name, t),
                &truncate_bytes(&instr, 200),
            )?;
            Ok(Some(json!({
                "task_id": task_id,
                "thread_id": thr,
                "node_id": node.id,
                "node_name": node.name,
                "node_kind": node.kind,
                "online": node.online,
            })))
        })
        .await?;
    if let Some(ref info) = res {
        hub.events.emit(user_id.to_owned(), "task", info.clone());
    }
    Ok(res)
}

#[derive(Deserialize)]
struct CompactionOutput {
    rolling_summary: String,
    #[serde(default)]
    memory_md: Option<String>,
    #[serde(default)]
    archive_thread_ids: Vec<String>,
}

const COMPACTION_SYSTEM_PROMPT: &str = r#"CONTEXT_COMPACTION_TASK: 你是个人智能体的后台上下文压缩与 Active Memory 剪枝器。
任务：
1. 把「旧历史摘要」与「待压缩对话批次」合并成一份紧凑的 `rolling_summary`（中文，不超过 1200 字）：
   - 保留：未完结事项、明确做出的决定、正在排查的项目上下文。
   - 剔除：寒暄、已经过期的一次性查询输出（如旧的 tmux 列表或临时日志）、重复过程。
2. 审视当前 `MEMORY`：
   - 仅当待压缩对话中出现新的长期偏好/规矩/环境事实，或 `MEMORY` 中有明显重复/过时条目时，输出整理后的完整 `memory_md`；否则填 `null`。
3. 审视活跃线程列表：把已经明确结束或过时的线程 ID 放进 `archive_thread_ids`（无则 `[]`）。

严格输出合法 JSON（勿加 Markdown 代码块）：
{"rolling_summary":"...","memory_md":null,"archive_thread_ids":[]}"#;

fn maybe_spawn_compaction(hub: Hub, user_id: String) {
    if !hub.try_begin_compaction(&user_id) {
        return;
    }
    tokio::spawn(async move {
        let _ = compact_user_context(&hub, &user_id).await;
        hub.end_compaction(&user_id);
    });
}

/// Incrementally compacts older completed messages into `agent_profiles.rolling_summary`,
/// prunes stale Active Memory entries, and archives finished/idle implicit threads.
pub async fn compact_user_context(hub: &Hub, user_id: &str) -> Result<bool> {
    let uid = user_id.to_owned();
    let candidate = hub
        .db
        .read(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let (old_summary, compacted_id) = read_compaction_state_sync(conn, &uid)?;
            let threads = list_active_threads_sync(conn, &uid)?;

            let mut stmt = conn.prepare(
                "SELECT id, role, content, thread_id FROM messages
                 WHERE user_id = ?1 AND status = 'complete' AND id > ?2
                 ORDER BY id ASC",
            )?;
            let uncompacted: Vec<(i64, String, String, Option<String>)> = stmt
                .query_map(params![uid, compacted_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            if uncompacted.len() < COMPACT_KEEP_RECENT + COMPACT_MIN_BATCH {
                return Ok(None);
            }
            let eligible = uncompacted.len() - COMPACT_KEEP_RECENT;
            let take_n = eligible.min(COMPACT_MAX_BATCH);
            let batch: Vec<(i64, String, String, Option<String>)> =
                uncompacted.into_iter().take(take_n).collect();
            let new_watermark = batch.last().map(|r| r.0).unwrap_or(compacted_id);

            Ok(Some((profile, old_summary, threads, batch, new_watermark)))
        })
        .await?;

    let Some((profile, old_summary, threads, batch, new_watermark)) = candidate else {
        // Even if message batch isn't large enough yet, quietly archive stale threads >48h idle.
        let uid = user_id.to_owned();
        let cutoff = now_unix().saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
        let _ = hub
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE threads SET status = 'archived'
                     WHERE user_id = ?1 AND status = 'active' AND updated_at < ?2",
                    params![uid, cutoff],
                )?;
                Ok(())
            })
            .await;
        return Ok(false);
    };

    let transcript = batch
        .iter()
        .map(|(_, role, content, thr_id)| {
            let tag = thr_id
                .as_deref()
                .map(|t| format!("[{t}] "))
                .unwrap_or_default();
            format!("{role}: {tag}{}", truncate_bytes(content.trim(), 480))
        })
        .collect::<Vec<_>>()
        .join("\n");

    let threads_txt = if threads.is_empty() {
        "（无）".into()
    } else {
        threads
            .iter()
            .map(|t| {
                format!(
                    "- {} | {} | {}",
                    t.id,
                    t.title,
                    truncate_bytes(&t.summary, 120)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let payload = format!(
        "【旧历史摘要】\n{}\n\n【当前 MEMORY】\n{}\n\n【活跃线程】\n{}\n\n【待压缩对话批次（共 {} 条）】\n{}",
        if old_summary.trim().is_empty() { "（空）" } else { old_summary.trim() },
        truncate_bytes(&profile.memory_md, 3000),
        threads_txt,
        batch.len(),
        truncate_bytes(&transcript, 8000),
    );

    let raw = crate::models::resolve(hub, user_id)
        .await?
        .brain
        .complete(COMPACTION_SYSTEM_PROMPT, &[], &payload, true)
        .await?;

    let trimmed = raw.trim();
    let parsed: Option<CompactionOutput> = trimmed
        .find('{')
        .zip(trimmed.rfind('}'))
        .and_then(|(s, e)| serde_json::from_str::<CompactionOutput>(&trimmed[s..=e]).ok());

    let Some(out) = parsed else {
        return Ok(false);
    };
    let clean_summary = truncate_bytes(out.rolling_summary.trim(), 2000);
    if clean_summary.is_empty() {
        return Ok(false);
    }
    let pruned_memory = out
        .memory_md
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| truncate_bytes(s, 8000));
    let archive_ids = out.archive_thread_ids;

    let uid = user_id.to_owned();
    let default_name = profile.name.clone();
    let default_soul = profile.soul_md.clone();
    let fallback_mem = profile.memory_md.clone();
    let memory_changed = pruned_memory.is_some();

    hub.db
        .write(move |conn| {
            let now = now_unix();
            let final_mem = pruned_memory.unwrap_or(fallback_mem);
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, rolling_summary, compacted_msg_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(user_id) DO UPDATE SET
                   memory_md = excluded.memory_md,
                   rolling_summary = excluded.rolling_summary,
                   compacted_msg_id = MAX(agent_profiles.compacted_msg_id, excluded.compacted_msg_id),
                   updated_at = excluded.updated_at",
                params![
                    uid,
                    default_name,
                    default_soul,
                    final_mem,
                    clean_summary,
                    new_watermark,
                    now
                ],
            )?;
            for tid in archive_ids {
                let t = tid.trim();
                if !t.is_empty() {
                    conn.execute(
                        "UPDATE threads SET status = 'archived', updated_at = ?3 WHERE id = ?1 AND user_id = ?2",
                        params![t, uid, now],
                    )?;
                }
            }
            let cutoff = now.saturating_sub(THREAD_IDLE_ARCHIVE_SECS);
            conn.execute(
                "UPDATE threads SET status = 'archived'
                 WHERE user_id = ?1 AND status = 'active' AND updated_at < ?2",
                params![uid, cutoff],
            )?;
            record_audit(
                conn,
                &uid,
                "context_compact",
                "ok",
                "自动压缩上下文摘要",
                &format!("watermark={new_watermark}"),
            )?;
            Ok(())
        })
        .await?;

    if memory_changed {
        hub.events
            .emit(user_id.to_owned(), "brain", json!({"updated": "profile"}));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::{parse_brain_response, strip_thread_tags};

    #[test]
    fn strips_leaked_thread_tags_only_at_line_start() {
        assert_eq!(strip_thread_tags("[thr_a4e693df] 图我收到了"), "图我收到了");
        assert_eq!(
            strip_thread_tags("[thr_a1] [thr_b2] 你好\n第二行"),
            "你好\n第二行"
        );
        assert_eq!(
            strip_thread_tags("引用 [thr_a1] 保留"),
            "引用 [thr_a1] 保留"
        );
        assert_eq!(
            strip_thread_tags("[thr_] 非法标记保留"),
            "[thr_] 非法标记保留"
        );
        assert_eq!(
            strip_thread_tags("- 列表\n  缩进保留"),
            "- 列表\n  缩进保留"
        );
    }

    #[test]
    fn recovers_bare_tool_call_with_hallucinated_xml_tag() {
        let parsed = parse_brain_response(r#"{"tool":"fetch_gitlab_commits"}</tool_call>"#);
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].tool, "fetch_gitlab_commits");
        assert!(!parsed.reply.contains("</tool_call>"));
        assert!(!parsed.reply.contains('{'));
    }

    #[test]
    fn recovers_prose_plus_tool_call_tag_and_strips_think() {
        let raw = r#"<think>internal</think>好的，我来查一下。<tool_call>{"tool":"dispatch_node","title":"查 Atlas 提交","instruction":"git log"}</tool_call>"#;
        let parsed = parse_brain_response(raw);
        assert_eq!(parsed.reply, "好的，我来查一下。");
        assert_eq!(parsed.tools.len(), 1);
        assert_eq!(parsed.tools[0].tool, "dispatch_node");
    }

    #[test]
    fn synthesizes_reply_when_tools_envelope_omits_reply() {
        let parsed = parse_brain_response(
            r#"{"reply":"","tools":[{"tool":"dispatch_node","title":"t","instruction":"i"}]}"#,
        );
        assert_eq!(parsed.tools.len(), 1);
        assert!(parsed.reply.contains("已派发给执行节点"));
    }
}
