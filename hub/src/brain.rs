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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub soul_md: String,
    pub memory_md: String,
    pub updated_at: i64,
}

impl Default for AgentProfile {
    fn default() -> Self {
        Self {
            name: DEFAULT_AGENT_NAME.into(),
            soul_md: DEFAULT_SOUL_MD.into(),
            memory_md: DEFAULT_MEMORY_MD.into(),
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
            "SELECT name, soul_md, memory_md, updated_at FROM agent_profiles WHERE user_id = ?1",
            [user_id],
            |r| {
                Ok(AgentProfile {
                    name: r.get(0)?,
                    soul_md: r.get(1)?,
                    memory_md: r.get(2)?,
                    updated_at: r.get(3)?,
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

/// Single initial payload for the frontend (replaces 7 parallel polling requests).
pub async fn bootstrap(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();

    let data = hub
        .db
        .read(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
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
                "SELECT t.id, t.requester_id, t.owner_id, t.title, t.instruction, t.status,
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
                        "title": r.get::<_, String>(3)?,
                        "instruction": r.get::<_, String>(4)?,
                        "status": r.get::<_, String>(5)?,
                        "result": r.get::<_, Option<String>>(6)?,
                        "revision": r.get::<_, i64>(7)?,
                        "updated_at": r.get::<_, i64>(8)?,
                        "node_id": r.get::<_, Option<String>>(9)?,
                        "job_status": r.get::<_, Option<String>>(10)?,
                        "stage": r.get::<_, Option<String>>(11)?,
                        "log_tail": r.get::<_, Option<String>>(12)?,
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
    pub name: String,
    pub soul_md: String,
    pub memory_md: String,
}

pub async fn save_profile(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<SaveProfileInput>,
) -> Result<Json<AgentProfile>> {
    let user = hub.require_user(&headers).await?;
    let name = truncate_bytes(input.name.trim(), 48);
    if name.is_empty() {
        return Err(invalid("agent name cannot be empty"));
    }
    let profile = AgentProfile {
        name,
        soul_md: truncate_bytes(input.soul_md.trim(), 6000),
        memory_md: truncate_bytes(input.memory_md.trim(), 8000),
        updated_at: now_unix(),
    };
    let p_db = profile.clone();
    let uid = user.id.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(user_id) DO UPDATE SET
                   name=excluded.name, soul_md=excluded.soul_md,
                   memory_md=excluded.memory_md, updated_at=excluded.updated_at",
                params![
                    uid,
                    p_db.name,
                    p_db.soul_md,
                    p_db.memory_md,
                    p_db.updated_at
                ],
            )?;
            Ok(())
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
pub struct ChatInput {
    pub message: String,
    #[serde(default)]
    pub request_id: Option<String>,
}

#[derive(Deserialize)]
struct BrainResponse {
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

/// Unified single-brain turn execution used by both Web UI and inbound IM webhook.
pub async fn run_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    request_id: Option<&str>,
) -> Result<Value> {
    // Idempotent replay if request_id already completed.
    if let Some(rid) = request_id {
        let uid = user_id.to_owned();
        let rid_s = rid.to_owned();
        let existing: Option<String> = hub
            .db
            .read(move |conn| {
                let user_msg_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM messages WHERE user_id=?1 AND request_id=?2",
                        params![uid, rid_s],
                        |r| r.get(0),
                    )
                    .optional()?;
                let Some(mid) = user_msg_id else {
                    return Ok(None);
                };
                Ok(conn
                    .query_row(
                        "SELECT content FROM messages WHERE user_id=?1 AND id > ?2 AND role='assistant' ORDER BY id ASC LIMIT 1",
                        params![uid, mid],
                        |r| r.get(0),
                    )
                    .optional()?)
            })
            .await?;
        if let Some(reply) = existing {
            return Ok(json!({"reply": reply, "replayed": true}));
        }
    }

    // Per-user turn serialization gate.
    let gate = hub.user_gate(user_id);
    let _guard = gate
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, "another chat turn is already running"))?;

    // Persist user message + load structured workbench snapshot cleanly in one DB read/write.
    let uid = user_id.to_owned();
    let msg_owned = message.to_owned();
    let rid_owned = request_id.map(str::to_owned);
    let snapshot = hub
        .db
        .write(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let (rolling_summary, _) = read_compaction_state_sync(conn, &uid)?;
            let threads = list_active_threads_sync(conn, &uid)?;
            let ledgers = list_ledgers_sync(conn, &uid)?;
            let nodes = nodes::list_nodes_sync(conn, &uid)?;
            let crons = automation::list_crons_sync(conn, &uid)?;
            let rules = automation::list_rules_sync(conn, &uid)?;
            let matched_rule = automation::match_rule_sync(conn, &uid, &msg_owned)?;

            let mut hist_stmt = conn.prepare(
                "SELECT role, content, thread_id FROM (
                    SELECT id, role, content, thread_id FROM messages
                    WHERE user_id = ?1 AND status = 'complete' AND role IN ('user','assistant')
                    ORDER BY id DESC LIMIT 10
                 ) ORDER BY id ASC",
            )?;
            let raw_hist: Vec<(String, String, Option<String>)> = hist_stmt
                .query_map([&uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let total_hist = raw_hist.len();
            let history: Vec<ChatMessage> = raw_hist
                .into_iter()
                .enumerate()
                .map(|(idx, (role, content, thr_id))| {
                    // Keep the newest 2 turns detailed; trim older turns tightly so long node outputs don't bloat context.
                    let budget = if idx + 2 >= total_hist { 1200 } else { 480 };
                    let trimmed = truncate_bytes(&content, budget);
                    let tagged = match thr_id.as_deref().filter(|s| !s.is_empty()) {
                        Some(tid) => format!("[{tid}] {trimmed}"),
                        None => trimmed,
                    };
                    ChatMessage {
                        role,
                        content: tagged,
                    }
                })
                .collect();

            let mut contact_stmt = conn.prepare(
                "SELECT username, display_name FROM users WHERE status='active' AND id != ?1 ORDER BY username ASC LIMIT 30",
            )?;
            let contacts: Vec<String> = contact_stmt
                .query_map([&uid], |r| {
                    Ok(format!("{} ({})", r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            conn.execute(
                "INSERT INTO messages(user_id, role, content, request_id, status, created_at)
                 VALUES (?1, 'user', ?2, ?3, 'complete', ?4)",
                params![uid, msg_owned, rid_owned, now_unix()],
            )?;
            let user_msg_id = conn.last_insert_rowid();

            Ok((
                profile,
                rolling_summary,
                threads,
                ledgers,
                nodes,
                crons,
                rules,
                matched_rule,
                history,
                contacts,
                user_msg_id,
            ))
        })
        .await?;

    let (
        profile,
        rolling_summary,
        threads,
        ledgers,
        node_list,
        crons,
        rules,
        matched_rule,
        history,
        contacts,
        user_msg_id,
    ) = snapshot;

    // Build structured system prompt with per-section budgets (system rules & tools are NEVER truncated).
    let system_prompt = build_system_prompt(
        &profile,
        &rolling_summary,
        &threads,
        &ledgers,
        &node_list,
        &crons,
        &rules,
        &contacts,
    );

    let raw_llm = hub
        .llm
        .complete(&system_prompt, &history, message, true)
        .await?;

    let parsed = parse_brain_response(&raw_llm);
    let mut final_reply = parsed.reply;
    let mut dispatched_tasks: Vec<Value> = Vec::new();
    let mut active_thread_id = resolve_or_create_thread(
        hub,
        user_id,
        parsed.thread_id.as_deref(),
        parsed.thread_title.as_deref(),
        message,
    )
    .await?;

    for call in parsed.tools {
        match call.tool.as_str() {
            "update_memory" => {
                apply_update_memory(hub, user_id, &call).await?;
            }
            "update_ledger" => {
                apply_update_ledger(hub, user_id, &call).await?;
            }
            "close_thread" => {
                if let Some(tid) = call
                    .thread_id
                    .as_deref()
                    .or(active_thread_id.as_deref())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    archive_thread(hub, user_id, tid).await?;
                }
            }
            "manage_cron" => {
                apply_manage_cron(hub, user_id, &call).await?;
            }
            "manage_rule" => {
                apply_manage_rule(hub, user_id, &call).await?;
            }
            "fetch_gitlab_commits" => {
                hub.events.emit(
                    user_id.to_owned(),
                    "stage",
                    json!({"stage": "正在拉取 GitLab 提交记录…"}),
                );
                if let Some(summary) =
                    crate::integrations::gitlab::fetch_commit_summary(hub, user_id).await
                {
                    final_reply.push_str("\n\n");
                    final_reply.push_str(&summary);
                }
            }
            "dispatch_node" => {
                hub.events.emit(
                    user_id.to_owned(),
                    "stage",
                    json!({"stage": "正在派发到执行节点…"}),
                );
                let instr = call
                    .instruction
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(message)
                    .to_owned();
                let title = call
                    .title
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| instr.lines().next().unwrap_or("节点任务"))
                    .to_owned();
                let preferred_node = call
                    .node_id
                    .as_deref()
                    .or_else(|| matched_rule.as_ref().map(|r| r.target_node_id.as_str()))
                    .map(str::to_owned);
                let prefer_cloud = call.prefer_cloud.unwrap_or(false);
                // Ensure every node dispatch belongs to an implicit backend thread for future session resume.
                let task_thread_id = if let Some(tid) = resolve_or_create_thread(
                    hub,
                    user_id,
                    call.thread_id.as_deref().or(active_thread_id.as_deref()),
                    Some(&title),
                    &title,
                )
                .await?
                {
                    active_thread_id = Some(tid.clone());
                    tid
                } else {
                    let created = create_thread(hub, user_id, &title).await?;
                    active_thread_id = Some(created.clone());
                    created
                };
                if let Some(info) = dispatch_to_node(
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
                    dispatched_tasks.push(info);
                } else {
                    final_reply.push_str(
                        "\n\n（当前没有已注册的 PC 或云端执行节点，请先在设置中接入节点。）",
                    );
                }
            }
            "delegate_colleague" => {
                if let Some(peer) = call
                    .peer
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let instr = call
                        .instruction
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(message);
                    match collab::delegate_to_peer(hub, user_id, peer, instr).await {
                        Ok((peer_name, task_id, mode)) => {
                            dispatched_tasks.push(json!({
                                "task_id": task_id,
                                "peer": peer_name,
                                "mode": mode,
                            }));
                        }
                        Err(_) => {
                            final_reply
                                .push_str(&format!("\n\n（未找到名为 `{peer}` 的活跃同事。）"));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Persist assistant reply + tag both messages with resolved implicit thread_id.
    let uid = user_id.to_owned();
    let reply_db = truncate_bytes(final_reply.trim(), 16_000);
    let reply_ret = reply_db.clone();
    let thr_db = active_thread_id.clone();
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
            let now = now_unix();
            if let Some(ref tid) = thr_db {
                conn.execute(
                    "UPDATE messages SET thread_id = ?1 WHERE id = ?2 AND user_id = ?3",
                    params![tid, user_msg_id, uid],
                )?;
                conn.execute(
                    "UPDATE threads
                     SET turn_count = turn_count + 1,
                         summary = CASE WHEN summary = '' THEN ?3 ELSE summary END,
                         updated_at = ?4
                     WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid, thr_summary_snippet, now],
                )?;
            }
            conn.execute(
                "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                 VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                params![uid, thr_db, reply_db, now],
            )?;
            Ok(())
        })
        .await?;

    hub.events
        .emit(user_id.to_owned(), "stage", json!({"stage": ""}));
    hub.events
        .emit(user_id.to_owned(), "message", json!({"done": true}));

    // Non-blocking background rolling context compaction & Active Memory pruning.
    maybe_spawn_compaction(hub.clone(), user_id.to_owned());

    Ok(json!({
        "reply": reply_ret,
        "thread_id": active_thread_id,
        "tasks": dispatched_tasks,
    }))
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
                    truncate_bytes(&n.capabilities, 120)
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

    format!(
        r#"你是用户的 7×24h 个人超级智能体「{name}」。请始终用第一人称、干练、结论先行的中文交流。

=== 自动语义归类、防串台与必要时反问（核心纪律） ===
- 用户只在单一聊天框里自然说话，不手动选线程；由你判断这句话在承接哪个隐式线程（`thread_id`）、还是开启新主题（`"new"`）、还是普通闲聊（`null`）。
- 近期对话中 `[thr_xxx]` 标注了每条消息所属的隐式线程，下方【隐式上下文线程】列出了活跃线程和是否可续接节点 session。
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

=== 输出格式（严格合法 JSON，勿加 Markdown 代码块） ===
{{"reply":"直接展示给用户的自然中文回复","thread_id":"thr_xxx 或 new 或 null","thread_title":"仅当 thread_id=new 时给简短标题，否则 null","tools":[]}}

=== 可组合调用的工具（tools 数组可为空或多项） ===
1. update_memory: {{"tool":"update_memory","name":null,"soul_md":null,"memory_append":"追加一条长期记忆/规矩","memory_replace":null}}
2. update_ledger: {{"tool":"update_ledger","action":"upsert|mute|done|delete","ledger_id":null,"title":"专项名","status":"active|muted|done","notes_append":"增量记录","notes_replace":null}}
3. manage_cron: {{"tool":"manage_cron","action":"add|delete","cron_id":null,"daily_time_cst":"HH:MM或null","interval_minutes":60,"target":"cloud|auto|node_id","instruction":"完整指令"}}
4. manage_rule: {{"tool":"manage_rule","action":"add|delete","rule_id":null,"keyword":"关键词","node_id":"目标节点或cloud"}}
5. fetch_gitlab_commits: {{"tool":"fetch_gitlab_commits"}}
6. dispatch_node: {{"tool":"dispatch_node","thread_id":"沿用的 thr_xxx 或 null","node_id":null,"prefer_cloud":false,"title":"简短标题","instruction":"补全全部上下文指代后的自包含执行指令"}}
   - 涉及本机代码/tmux/私有环境时 prefer_cloud=false；通用云端脚本/分析/爬虫/截图任务可设 prefer_cloud=true。
7. close_thread: {{"tool":"close_thread","thread_id":"已完结的 thr_xxx"}}
8. delegate_colleague: {{"tool":"delegate_colleague","peer":"同事username或姓名","instruction":"需要同事协助的清晰事项"}}"#,
        name = profile.name,
    )
}

fn parse_brain_response(raw: &str) -> BrainResponse {
    let trimmed = raw.trim();
    if let (Some(s), Some(e)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if let Ok(parsed) = serde_json::from_str::<BrainResponse>(&trimmed[s..=e]) {
            if !parsed.reply.trim().is_empty() {
                return parsed;
            }
        }
    }
    BrainResponse {
        reply: trimmed.to_owned(),
        thread_id: None,
        thread_title: None,
        tools: Vec::new(),
    }
}

async fn apply_update_memory(hub: &Hub, user_id: &str, call: &ToolCall) -> Result<()> {
    let uid = user_id.to_owned();
    let name_opt = call.name.clone();
    let soul_opt = call.soul_md.clone();
    let mem_rep = call.memory_replace.clone();
    let mem_app = call.memory_append.clone();
    hub.db
        .write(move |conn| {
            let mut p = read_profile_sync(conn, &uid)?;
            if let Some(n) = name_opt.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                p.name = truncate_bytes(n, 48);
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
                "INSERT INTO agent_profiles(user_id, name, soul_md, memory_md, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(user_id) DO UPDATE SET
                   name=excluded.name, soul_md=excluded.soul_md,
                   memory_md=excluded.memory_md, updated_at=excluded.updated_at",
                params![uid, p.name, p.soul_md, p.memory_md, p.updated_at],
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

    let raw = hub
        .llm
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
