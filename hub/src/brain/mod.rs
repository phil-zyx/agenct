//! Unified single-brain agent loop (SOUL + MEMORY + Tools + Streaming) & bootstrap API.
mod compact;
mod parser;
mod prompt;
mod roles;
mod threads;
mod tools;

pub use compact::compact_user_context;
pub use roles::{delete_role, list_roles, list_roles_sync, AgentRole};
pub use threads::{archive_stale_threads, list_active_threads_sync, list_threads_sync, ThreadView};

use crate::{
    auth::AuthUser,
    automation,
    config::{now_unix, truncate_bytes, truncate_head_tail_bytes},
    db::record_audit,
    denied, invalid,
    llm::ChatMessage,
    nodes,
    secrets::{SecretCipher, SCOPE_GITLAB},
    Error, Hub, Result,
};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use parser::{
    default_reply_for_tools, native_calls_to_tool_calls, recover_inline_tool_calls,
    strip_thread_tags,
};
use prompt::{
    build_system_prompt, tool_definitions, DEFAULT_AGENT_NAME, DEFAULT_MEMORY_MD, DEFAULT_SOUL_MD,
    VISION_CAPTION_PROMPT,
};
use rusqlite::{params, params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tools::{execute_tool, ToolOutcome, TOOL_OBSERVATION_MAX_BYTES};

pub const MAX_CALLBACK_DEPTH: i64 = 3;
const DEFAULT_AVATAR_ID: &str = "agenct";

fn normalize_avatar_id(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "agenct" | "default" | "默认" => Some("agenct"),
        "buddy" | "muse" | "动态管家" | "状态管家" => Some("buddy"),
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

/// Single initial payload for the frontend (replaces 7 parallel polling requests).
pub async fn bootstrap(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let uid = user.id.clone();

    let data = hub
        .db
        .read(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let roles = list_roles_sync(conn, &uid)?;
            let threads = list_threads_sync(conn, &uid)?;
            let node_list = nodes::list_nodes_sync(conn, &uid)?;
            let permissions = automation::read_permissions_sync(conn, &uid)?;
            let rules = automation::list_rules_sync(conn, &uid)?;
            let crons = automation::list_crons_sync(conn, &uid)?;

            let mut msg_stmt = conn.prepare(
                "SELECT id, peer_id, thread_id, role, content, request_id, status, reply_to, created_at FROM (
                    SELECT id, peer_id, thread_id, role, content, request_id, status, reply_to, created_at
                    FROM messages WHERE user_id = ?1
                    ORDER BY id DESC LIMIT 61
                 ) ORDER BY id ASC",
            )?;
            let mut messages: Vec<Value> = msg_stmt
                .query_map([&uid], |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "peer_id": r.get::<_, Option<String>>(1)?,
                        "thread_id": r.get::<_, Option<String>>(2)?,
                        "role": r.get::<_, String>(3)?,
                        "content": r.get::<_, String>(4)?,
                        "request_id": r.get::<_, Option<String>>(5)?,
                        "status": r.get::<_, String>(6)?,
                        "reply_to": r.get::<_, Option<i64>>(7)?,
                        "created_at": r.get::<_, i64>(8)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let has_more_messages = messages.len() > 60;
            if has_more_messages {
                messages.remove(0);
            }

            let mut task_stmt = conn.prepare(
                "SELECT t.id, t.requester_id, t.owner_id, t.thread_id, t.title, t.instruction, t.status,
                        t.result, t.blocked_reason, t.revision, t.updated_at,
                        j.node_id, j.status, j.stage, j.log_tail, j.role, j.agent, j.model, j.sandbox
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
                        "blocked_reason": r.get::<_, String>(8)?,
                        "revision": r.get::<_, i64>(9)?,
                        "updated_at": r.get::<_, i64>(10)?,
                        "node_id": r.get::<_, Option<String>>(11)?,
                        "job_status": r.get::<_, Option<String>>(12)?,
                        "stage": r.get::<_, Option<String>>(13)?,
                        "log_tail": r.get::<_, Option<String>>(14)?,
                        "role": r.get::<_, Option<String>>(15)?,
                        "agent": r.get::<_, Option<String>>(16)?,
                        "model": r.get::<_, Option<String>>(17)?,
                        "sandbox": r.get::<_, Option<String>>(18)?,
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
                "roles": roles,
                "threads": threads,
                "messages": messages,
                "has_more_messages": has_more_messages,
                "tasks": tasks,
                "nodes": node_list,
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
pub struct ListMessagesQuery {
    pub before_id: Option<i64>,
    pub thread_id: Option<String>,
    pub limit: Option<i64>,
}

/// Cursor-based message pagination with optional `thread_id` filtering.
pub async fn list_messages(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Query(q): Query<ListMessagesQuery>,
) -> Result<Json<Value>> {
    let uid = user.id;
    let limit = q.limit.unwrap_or(40).clamp(1, 100);
    let fetch_limit = limit + 1;
    let before_id = q.before_id.filter(|&id| id > 0);
    let thread_id = q
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let (messages, has_more, next_before_id) = hub
        .db
        .read(move |conn| {
            let mut sql = String::from(
                "SELECT id, peer_id, thread_id, role, content, request_id, status, reply_to, created_at
                 FROM messages WHERE user_id = ?1",
            );
            let mut args: Vec<SqlValue> = vec![SqlValue::Text(uid)];
            if let Some(bid) = before_id {
                args.push(SqlValue::Integer(bid));
                sql.push_str(&format!(" AND id < ?{}", args.len()));
            }
            if let Some(tid) = thread_id {
                args.push(SqlValue::Text(tid));
                sql.push_str(&format!(" AND thread_id = ?{}", args.len()));
            }
            args.push(SqlValue::Integer(fetch_limit));
            sql.push_str(&format!(" ORDER BY id DESC LIMIT ?{}", args.len()));

            let mut stmt = conn.prepare(&sql)?;
            let mut rows: Vec<Value> = stmt
                .query_map(params_from_iter(args), |r| {
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "peer_id": r.get::<_, Option<String>>(1)?,
                        "thread_id": r.get::<_, Option<String>>(2)?,
                        "role": r.get::<_, String>(3)?,
                        "content": r.get::<_, String>(4)?,
                        "request_id": r.get::<_, Option<String>>(5)?,
                        "status": r.get::<_, String>(6)?,
                        "reply_to": r.get::<_, Option<i64>>(7)?,
                        "created_at": r.get::<_, i64>(8)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let has_more = rows.len() > limit as usize;
            if has_more {
                rows.pop();
            }
            rows.reverse();
            let next_before_id = rows.first().and_then(|v| v["id"].as_i64());
            Ok((rows, has_more, next_before_id))
        })
        .await?;

    Ok(Json(json!({
        "messages": messages,
        "has_more": has_more,
        "next_before_id": next_before_id,
    })))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    pub thread_id: Option<String>,
    pub limit: Option<i64>,
}

fn build_snippet(content: &str, context: &str, terms: &[String]) -> String {
    let combined = if context.trim().is_empty() {
        content.trim().to_owned()
    } else {
        format!("{} {}", content.trim(), context.trim())
    };
    let lower = combined.to_lowercase();
    let first_hit = terms
        .iter()
        .filter_map(|t| lower.find(&t.to_lowercase()))
        .min()
        .unwrap_or(0);
    let start_approx = first_hit.min(combined.len()).saturating_sub(40);
    let start = (0..=start_approx)
        .rev()
        .find(|&i| combined.is_char_boundary(i))
        .unwrap_or(0);
    let slice = &combined[start..];
    let mut out = truncate_bytes(slice, 180);
    if start > 0 {
        out = format!("…{out}");
    }
    if slice.len() > 180 {
        out.push('…');
    }
    out
}

/// Searches messages (FTS5 trigram with substring fallback for <3-char terms) and memory.
pub async fn search(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>> {
    let query_str = q.q.trim().to_owned();
    if query_str.is_empty() || query_str.len() > 200 {
        return Err(invalid("search query must be 1..200 bytes"));
    }
    let terms: Vec<String> = query_str
        .split_whitespace()
        .take(6)
        .map(str::to_owned)
        .collect();
    if terms.is_empty() {
        return Err(invalid("search query cannot be empty"));
    }
    let uid = user.id;
    let limit = q.limit.unwrap_or(20).clamp(1, 50);
    let thread_id = q
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let payload = hub
        .db
        .read(move |conn| {
            let use_fts = terms.iter().all(|t| t.chars().count() >= 3);
            let mut sql = String::from(
                "SELECT m.id, m.thread_id, m.role, m.content, m.context, m.created_at
                 FROM messages m ",
            );
            let mut args: Vec<SqlValue> = Vec::new();
            if use_fts {
                let fts_expr = terms
                    .iter()
                    .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(" ");
                sql.push_str(
                    "JOIN messages_fts ON messages_fts.rowid = m.id
                     WHERE m.user_id = ?1 AND m.status != 'failed' AND messages_fts MATCH ?2",
                );
                args.push(SqlValue::Text(uid.clone()));
                args.push(SqlValue::Text(fts_expr));
            } else {
                sql.push_str("WHERE m.user_id = ?1 AND m.status != 'failed'");
                args.push(SqlValue::Text(uid.clone()));
                for term in &terms {
                    args.push(SqlValue::Text(term.to_lowercase()));
                    sql.push_str(&format!(
                        " AND instr(lower(m.content || ' ' || m.context), ?{}) > 0",
                        args.len()
                    ));
                }
            }
            if let Some(tid) = thread_id {
                args.push(SqlValue::Text(tid));
                sql.push_str(&format!(" AND m.thread_id = ?{}", args.len()));
            }
            args.push(SqlValue::Integer(limit));
            sql.push_str(&format!(" ORDER BY m.id DESC LIMIT ?{}", args.len()));

            let mut msg_stmt = conn.prepare(&sql)?;
            let messages: Vec<Value> = msg_stmt
                .query_map(params_from_iter(args), |r| {
                    let content: String = r.get(3)?;
                    let context: String = r.get(4)?;
                    Ok(json!({
                        "id": r.get::<_, i64>(0)?,
                        "thread_id": r.get::<_, Option<String>>(1)?,
                        "role": r.get::<_, String>(2)?,
                        "snippet": build_snippet(&content, &context, &terms),
                        "created_at": r.get::<_, i64>(5)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

            let lower_terms: Vec<String> = terms.iter().map(|t| t.to_lowercase()).collect();
            let profile = read_profile_sync(conn, &uid)?;
            let memory_matches: Vec<String> = profile
                .memory_md
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .filter(|line| {
                    let hay = line.to_lowercase();
                    lower_terms.iter().all(|t| hay.contains(t))
                })
                .take(10)
                .map(|line| truncate_bytes(line, 200))
                .collect();

            Ok(json!({
                "query": query_str,
                "messages": messages,
                "memory_matches": memory_matches,
            }))
        })
        .await?;

    Ok(Json(payload))
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
    AuthUser(user): AuthUser,
    Json(input): Json<SaveProfileInput>,
) -> Result<Json<AgentProfile>> {
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
pub struct UpdateThreadInput {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

pub async fn update_thread(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
    Json(input): Json<UpdateThreadInput>,
) -> Result<Json<Value>> {
    if input.status.is_none() && input.title.is_none() {
        return Err(invalid("at least one of status or title is required"));
    }
    let next_status = match input.status.as_deref() {
        Some(raw) => {
            let s = raw.trim().to_owned();
            if !matches!(s.as_str(), "active" | "archived") {
                return Err(invalid("thread status must be active or archived"));
            }
            Some(s)
        }
        None => None,
    };
    let next_title = match input.title.as_deref() {
        Some(raw) => {
            let clean = truncate_bytes(raw.lines().next().unwrap_or("").trim(), 80);
            if clean.is_empty() {
                return Err(invalid("thread title cannot be empty"));
            }
            Some(clean)
        }
        None => None,
    };

    let uid = user.id.clone();
    let tid = id.clone();
    let st_ret = next_status.clone();
    let title_ret = next_title.clone();
    hub.db
        .write(move |conn| {
            let cur: Option<(String, String)> = conn
                .query_row(
                    "SELECT status, title FROM threads WHERE id = ?1 AND user_id = ?2",
                    params![tid, uid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((cur_status, cur_title)) = cur else {
                return Err(denied());
            };
            conn.execute(
                "UPDATE threads
                 SET status = ?3, title = ?4, updated_at = ?5
                 WHERE id = ?1 AND user_id = ?2",
                params![
                    tid,
                    uid,
                    next_status.unwrap_or(cur_status),
                    next_title.unwrap_or(cur_title),
                    now_unix()
                ],
            )?;
            Ok(())
        })
        .await?;
    hub.events.emit(
        user.id,
        "brain",
        json!({
            "updated_thread": id,
            "status": st_ret,
            "title": title_ret,
        }),
    );
    Ok(Json(json!({"ok": true, "id": id})))
}

pub async fn delete_thread(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
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
    #[serde(default)]
    pub thread_id: Option<String>,
}

pub async fn chat(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<ChatInput>,
) -> Result<(StatusCode, Json<Value>)> {
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
    let pinned_tid = input
        .thread_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(ref tid) = pinned_tid {
        let uid = user.id.clone();
        let tid_db = tid.clone();
        let owned: bool = hub
            .db
            .read(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT 1 FROM threads WHERE id = ?1 AND user_id = ?2",
                        params![tid_db, uid],
                        |_| Ok(true),
                    )
                    .optional()?
                    .unwrap_or(false))
            })
            .await?;
        if !owned {
            return Err(invalid("invalid thread_id"));
        }
    }
    match start_turn(
        &hub,
        &user.id,
        msg,
        input.request_id.as_deref(),
        pinned_tid.as_deref(),
    )
    .await?
    {
        StartTurnResult::Replay(val) => Ok((StatusCode::OK, Json(val))),
        StartTurnResult::Started {
            message_id,
            handle: _,
        } => Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "accepted",
                "message_id": message_id,
                "request_id": input.request_id,
            })),
        )),
    }
}

/// Maximum LLM calls per chat turn (initial call + tool-result follow-ups).
const MAX_AGENT_STEPS: usize = 3;

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
    roles: Vec<AgentRole>,
    threads: Vec<ThreadView>,
    nodes: Vec<nodes::NodeView>,
    crons: Vec<automation::CronJob>,
    rules: Vec<automation::DispatchRule>,
    matched_rule: Option<automation::DispatchRule>,
    gitlab_configured: bool,
    history: Vec<ChatMessage>,
    contacts: Vec<String>,
    pinned_thread_title: Option<String>,
    user_msg_id: i64,
}

fn parse_approve_command(message: &str) -> Option<String> {
    let (prefix, rest) = message.trim().split_once(char::is_whitespace)?;
    if !prefix.eq_ignore_ascii_case("/approve") && prefix != "放行" {
        return None;
    }
    let code = rest.trim();
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Some(code.to_ascii_uppercase())
    } else {
        None
    }
}

struct PendingTurnGuard {
    hub: Hub,
    user_id: String,
    user_msg_id: i64,
    request_id: Option<String>,
    finished: bool,
}

impl Drop for PendingTurnGuard {
    fn drop(&mut self) {
        if !self.finished {
            let hub = self.hub.clone();
            let uid = self.user_id.clone();
            let mid = self.user_msg_id;
            let rid = self.request_id.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    mark_turn_failed(&hub, &uid, mid, rid.as_deref(), "chat turn aborted").await;
                });
            }
        }
    }
}

/// Immediate result of initiating a single-brain turn (`Replay` vs detached `Started` handle).
pub enum StartTurnResult {
    Replay(Value),
    Started {
        message_id: i64,
        handle: tokio::task::JoinHandle<Result<Value>>,
    },
}

/// Starts a single-brain turn in a detached background task after synchronously claiming the
/// `pending` user message row in SQLite. Callers can either return immediately (`202 Accepted`
/// for HTTP `/api/chat`) or `.await` the returned `JoinHandle` (`run_turn` for IM & tests).
pub async fn start_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    request_id: Option<&str>,
    pinned_thread_id: Option<&str>,
) -> Result<StartTurnResult> {
    if let Some(rid) = request_id {
        match lookup_request(hub, user_id, rid).await? {
            RequestState::Replay(reply) => {
                return Ok(StartTurnResult::Replay(json!({
                    "status": "complete",
                    "reply": reply,
                    "replayed": true,
                })));
            }
            RequestState::InFlight => {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "request is already being processed",
                ))
            }
            RequestState::New | RequestState::Failed => {}
        }
    }

    // Per-user turn serialization gate (owned lock moves into the detached task).
    let gate = hub.user_gate(user_id);
    let guard = gate
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, "another chat turn is already running"))?;

    let (claimed_tx, claimed_rx) = tokio::sync::oneshot::channel::<Result<i64>>();
    let hub_owned = hub.clone();
    let uid_owned = user_id.to_owned();
    let msg_owned = message.to_owned();
    let rid_owned = request_id.map(str::to_owned);
    let pinned_owned = pinned_thread_id.map(str::to_owned);

    let handle = tokio::spawn(async move {
        run_turn_detached(
            guard,
            claimed_tx,
            hub_owned,
            uid_owned,
            msg_owned,
            rid_owned,
            pinned_owned,
        )
        .await
    });

    match claimed_rx.await {
        Ok(Ok(message_id)) => Ok(StartTurnResult::Started { message_id, handle }),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "chat turn aborted",
        )),
    }
}

/// Unified single-brain turn execution used by inbound IM webhooks and synchronous callers.
pub async fn run_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    request_id: Option<&str>,
    pinned_thread_id: Option<&str>,
) -> Result<Value> {
    match start_turn(hub, user_id, message, request_id, pinned_thread_id).await? {
        StartTurnResult::Replay(val) => Ok(val),
        StartTurnResult::Started { handle, .. } => match handle.await {
            Ok(res) => res,
            Err(join_err) => {
                eprintln!("[brain] turn task panicked for user {user_id}: {join_err}");
                Err(Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "chat turn aborted",
                ))
            }
        },
    }
}

async fn run_turn_detached(
    _guard: tokio::sync::OwnedMutexGuard<()>,
    claimed_tx: tokio::sync::oneshot::Sender<Result<i64>>,
    hub: Hub,
    user_id: String,
    message: String,
    request_id: Option<String>,
    pinned_thread_id: Option<String>,
) -> Result<Value> {
    let snapshot = match load_snapshot_and_claim(
        &hub,
        &user_id,
        &message,
        request_id.as_deref(),
        pinned_thread_id.as_deref(),
    )
    .await
    {
        Ok(snap) => snap,
        Err(err) => {
            let _ = claimed_tx.send(Err(Error(err.0, err.1)));
            return Err(err);
        }
    };
    let user_msg_id = snapshot.user_msg_id;
    let mut turn_guard = PendingTurnGuard {
        hub: hub.clone(),
        user_id: user_id.clone(),
        user_msg_id,
        request_id: request_id.clone(),
        finished: false,
    };
    hub.events.emit(
        user_id.clone(),
        "message",
        json!({
            "pending": true,
            "message_id": user_msg_id,
            "request_id": request_id,
        }),
    );
    let _ = claimed_tx.send(Ok(user_msg_id));

    if let Some(code) = parse_approve_command(&message) {
        let res = match crate::collab::consume_approval_code(&hub, &user_id, &code).await {
            Ok(outcome) => {
                let reply = format!(
                    "已授权单次放行重试「{}」，已重新派发到节点 {}（仅本次生效）。",
                    outcome.title, outcome.node_name
                );
                let tid = outcome.thread_id.or_else(|| pinned_thread_id.clone());
                persist_turn(
                    &hub,
                    &user_id,
                    &message,
                    user_msg_id,
                    request_id.as_deref(),
                    tid,
                    &reply,
                    "",
                )
                .await
                .map(|final_reply| {
                    json!({
                        "reply": final_reply,
                        "approved_task_id": outcome.task_id,
                    })
                })
            }
            Err(err) => {
                let reply = format!("单次放行未生效：{}。", err.1);
                persist_turn(
                    &hub,
                    &user_id,
                    &message,
                    user_msg_id,
                    request_id.as_deref(),
                    pinned_thread_id.clone(),
                    &reply,
                    "",
                )
                .await
                .map(|final_reply| json!({"reply": final_reply}))
            }
        };
        return match res {
            Ok(out) => {
                turn_guard.finished = true;
                Ok(out)
            }
            Err(err) => {
                eprintln!(
                    "[brain] turn failed user={} msg_id={} status={} err={}",
                    user_id,
                    user_msg_id,
                    err.0.as_u16(),
                    err.1
                );
                mark_turn_failed(&hub, &user_id, user_msg_id, request_id.as_deref(), err.1).await;
                turn_guard.finished = true;
                Err(err)
            }
        };
    }

    match drive_turn(
        &hub,
        &user_id,
        &message,
        snapshot,
        request_id.as_deref(),
        pinned_thread_id.as_deref(),
    )
    .await
    {
        Ok(out) => {
            turn_guard.finished = true;
            Ok(out)
        }
        Err(err) => {
            eprintln!(
                "[brain] turn failed user={} msg_id={} status={} err={}",
                user_id,
                user_msg_id,
                err.0.as_u16(),
                err.1
            );
            mark_turn_failed(&hub, &user_id, user_msg_id, request_id.as_deref(), err.1).await;
            turn_guard.finished = true;
            Err(err)
        }
    }
}

/// Reads everything the brain needs for one turn (`user_msg_id` is filled in by the caller).
fn build_snapshot_sync(
    conn: &Connection,
    secrets: &SecretCipher,
    uid: &str,
    message: &str,
    with_history: bool,
    pinned_thread_id: Option<&str>,
) -> Result<TurnSnapshot> {
    let profile = read_profile_sync(conn, uid)?;
    let (rolling_summary, _) = read_compaction_state_sync(conn, uid)?;
    let roles = list_roles_sync(conn, uid)?;
    let threads = list_active_threads_sync(conn, uid)?;
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
        .and_then(|s| secrets.decrypt(SCOPE_GITLAB, uid, &s).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["token"].as_str().map(|t| !t.trim().is_empty()))
        .unwrap_or(false);

    let pinned_thread_title = match pinned_thread_id {
        Some(tid) => conn
            .query_row(
                "SELECT title FROM threads WHERE id = ?1 AND user_id = ?2",
                params![tid, uid],
                |r| r.get::<_, String>(0),
            )
            .optional()?,
        None => None,
    };

    let history = if !with_history {
        Vec::new()
    } else {
        let mut raw_hist: Vec<(String, String, Option<String>, String)> = Vec::new();
        if let Some(tid) = pinned_thread_id {
            let mut thr_stmt = conn.prepare(
                "SELECT role, content, thread_id, context FROM (
                    SELECT id, role, content, thread_id, context FROM messages
                    WHERE user_id = ?1 AND thread_id = ?2 AND status = 'complete' AND role IN ('user','assistant')
                    ORDER BY id DESC LIMIT 10
                 ) ORDER BY id ASC",
            )?;
            raw_hist = thr_stmt
                .query_map(params![uid, tid], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        if raw_hist.len() < 2 {
            let mut hist_stmt = conn.prepare(
                "SELECT role, content, thread_id, context FROM (
                    SELECT id, role, content, thread_id, context FROM messages
                    WHERE user_id = ?1 AND status = 'complete' AND role IN ('user','assistant')
                    ORDER BY id DESC LIMIT 10
                 ) ORDER BY id ASC",
            )?;
            raw_hist = hist_stmt
                .query_map([uid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        let total_hist = raw_hist.len();
        raw_hist
            .into_iter()
            .enumerate()
            .map(|(idx, (role, content, thr_id, context))| {
                let full = if context.trim().is_empty() {
                    content
                } else {
                    format!("{content}\n{}", context.trim())
                };
                // Keep the newest 6 turns rich and preserve both head & tail so trailing action items aren't lost.
                let trimmed = if idx + 6 >= total_hist {
                    if full.len() <= 1800 {
                        full
                    } else {
                        truncate_head_tail_bytes(&full, 700, 900)
                    }
                } else if full.len() <= 800 {
                    full
                } else {
                    truncate_head_tail_bytes(&full, 300, 450)
                };
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
        roles,
        threads,
        nodes,
        crons,
        rules,
        matched_rule,
        gitlab_configured,
        history,
        contacts,
        pinned_thread_title,
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
    pinned_thread_id: Option<&str>,
) -> Result<TurnSnapshot> {
    let uid = user_id.to_owned();
    let msg_owned = message.to_owned();
    let rid_owned = request_id.map(str::to_owned);
    let pinned_owned = pinned_thread_id.map(str::to_owned);
    let secrets = hub.secrets.clone();
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut snap = build_snapshot_sync(
                &tx,
                &secrets,
                &uid,
                &msg_owned,
                true,
                pinned_owned.as_deref(),
            )?;
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
                        "UPDATE messages SET status='pending', content=?2, thread_id=?3, created_at=?4
                         WHERE id=?1",
                        params![id, msg_owned, pinned_owned, now],
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
                        "INSERT INTO messages(user_id, thread_id, role, content, request_id, status, created_at)
                         VALUES (?1, ?2, 'user', ?3, ?4, 'pending', ?5)",
                        params![uid, pinned_owned, msg_owned, rid_owned, now],
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

async fn mark_turn_failed(
    hub: &Hub,
    user_id: &str,
    user_msg_id: i64,
    request_id: Option<&str>,
    error_msg: &str,
) {
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
    hub.events.emit(user_id, "delta", json!({"done": true}));
    hub.events.emit(user_id, "stage", json!({"stage": ""}));
    hub.events.emit(
        user_id,
        "message",
        json!({
            "failed": true,
            "message_id": user_msg_id,
            "request_id": request_id,
            "error": error_msg,
        }),
    );
}

/// Mutable per-turn state shared across agent steps.
pub(super) struct TurnContext<'a> {
    pub(super) hub: &'a Hub,
    pub(super) user_id: &'a str,
    pub(super) message: &'a str,
    pub(super) node_list: &'a [nodes::NodeView],
    pub(super) matched_rule_target: Option<String>,
    pub(super) active_thread_id: Option<String>,
    pub(super) dispatched_tasks: Vec<Value>,
    /// Guaranteed `(code, markdown_note)` prompts for pending one-shot elevation requests.
    pub(super) approval_prompts: Vec<(String, String)>,
    /// Keys of side-effecting calls that already succeeded this turn (prevents follow-up
    /// steps from re-dispatching the same job).
    pub(super) done_keys: HashSet<String>,
    /// Interactive chat turns stream `stage` and `delta` progress to the UI; headless runs stay silent.
    pub(super) interactive: bool,
    /// Optional tool allow-list (headless/cron runs must not e.g. create more crons).
    pub(super) allowed_tools: Option<&'a [&'a str]>,
    /// Current async callback recursion depth (`0` for interactive chat & cron).
    pub(super) callback_depth: i64,
}

impl TurnContext<'_> {
    pub(super) fn stage(&self, text: &str) {
        if self.interactive {
            self.hub
                .events
                .emit(self.user_id, "stage", json!({ "stage": text }));
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
    callback_depth: i64,
    initial_thread_id: Option<String>,
}

/// Runs an interactive chat turn: agent loop, then atomically persists the final reply.
async fn drive_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    snap: TurnSnapshot,
    request_id: Option<&str>,
    pinned_thread_id: Option<&str>,
) -> Result<Value> {
    let focus_suffix = match pinned_thread_id {
        Some(tid) => {
            let title = snap.pinned_thread_title.as_deref().unwrap_or(tid);
            format!(
                "\n\n=== 当前聚焦 Topic ===\n\
                 用户正在工作线程 `{tid}`（「{title}」）界面下聚焦讨论，请优先围绕该 Topic 回答，并在调用 `dispatch_node` 时默认沿用 `{tid}`（除非用户明确要求另开全新无关话题）。"
            )
        }
        None => String::new(),
    };
    let mode = AgentMode {
        prompt_suffix: &focus_suffix,
        interactive: true,
        allowed_tools: None,
        callback_depth: 0,
        initial_thread_id: pinned_thread_id.map(str::to_owned),
    };
    let out = agent_loop(hub, user_id, message, &snap, &mode).await?;
    let reply = persist_turn(
        hub,
        user_id,
        message,
        snap.user_msg_id,
        request_id,
        out.thread_id.clone(),
        &out.reply,
        &out.user_context,
    )
    .await?;
    // Non-blocking background rolling context compaction & Active Memory pruning.
    compact::maybe_spawn_compaction(hub.clone(), user_id.to_owned());
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
    let secrets = hub.secrets.clone();
    let snap = hub
        .db
        .read(move |conn| build_snapshot_sync(conn, &secrets, &uid, &instr, false, None))
        .await?;
    let mode = AgentMode {
        prompt_suffix,
        interactive: false,
        allowed_tools: Some(allowed_tools),
        callback_depth: 0,
        initial_thread_id: None,
    };
    agent_loop(hub, user_id, instruction, &snap, &mode).await
}

fn append_missing_attachments(reply: &str, source: &str) -> String {
    let missing: Vec<String> = crate::attachments::extract_attachment_ids(source)
        .into_iter()
        .filter(|id| !reply.contains(id.as_str()))
        .collect();
    if missing.is_empty() {
        return reply.to_owned();
    }
    let mut out = reply.to_owned();
    for id in missing {
        let needle = format!("/api/attachments/{id})");
        let snippet = source
            .lines()
            .map(str::trim)
            .find(|line| line.contains(&needle))
            .map(str::to_owned)
            .unwrap_or_else(|| format!("[附件](/api/attachments/{id})"));
        if !out.trim().is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&snippet);
    }
    out
}

/// Returns `true` when raw node output is long or structured enough to benefit from
/// automatic Brain distillation rather than dumping raw prose directly into chat.
pub fn should_auto_digest(content: &str) -> bool {
    let mut non_empty_lines = 0usize;
    let mut char_count = 0usize;

    for raw_line in content.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Evidence attachment links are preserved separately in the distilled message.
        if (trimmed.starts_with("![") || trimmed.starts_with('['))
            && trimmed.contains("/api/attachments/att_")
        {
            continue;
        }
        non_empty_lines += 1;
        char_count += trimmed.chars().count();
        if non_empty_lines > 4
            || char_count > 260
            || trimmed.starts_with("```")
            || trimmed.starts_with("##")
        {
            return true;
        }
    }
    false
}

/// Invokes the brain asynchronously when a node completes a task:
/// either running an explicit `on_complete` follow-up workflow or automatically
/// distilling verbose node output into a concise chat reply.
#[allow(clippy::too_many_arguments)]
pub async fn run_task_callback(
    hub: Hub,
    user_id: String,
    task_id: String,
    task_title: String,
    node_name: String,
    task_status: String,
    result_content: String,
    on_complete: String,
    task_thread_id: Option<String>,
    callback_depth: i64,
    approval_footer: Option<String>,
) {
    let next_depth = callback_depth + 1;
    if next_depth > MAX_CALLBACK_DEPTH {
        return;
    }

    let gate = hub.user_gate(&user_id);
    let _guard = gate.lock_owned().await;

    let has_followup = !on_complete.trim().is_empty();
    let allow_followup_dispatch = has_followup && next_depth < MAX_CALLBACK_DEPTH;
    let allowed_tools: &[&str] = if !has_followup {
        &[]
    } else if allow_followup_dispatch {
        &["update_memory", "manage_thread", "dispatch_node"]
    } else {
        &["update_memory", "manage_thread"]
    };

    let truncated_result = truncate_bytes(result_content.trim(), 6000);
    let (instruction, prompt_suffix, audit_action, msg_header, im_title) = if has_followup {
        let instr = format!(
            "{}\n任务「{}」（id=`{}`，执行节点：{}，状态：{}）已返回结果。\n\
             既定后续验收/跟进要求：{}\n\n\
             【节点执行结果】\n{}\n\n\
             请用自然简洁的中文总结给用户（3～5 句、≤6 行以内）：直接说清结果与关键事实，最后如有具体文件/commit/任务ID/附件或链接顺手带一句（没有就别凑）；禁止套用「待审批：」「结论：」「来源：」这种机械前缀；节点原始报告已保存在右侧抽屉「查看结果」，切勿重复粘贴；如需要可调用允许的工具更新长期记忆、归档线程{}。",
            crate::llm::TASK_CALLBACK_MARKER,
            task_title,
            task_id,
            node_name,
            task_status,
            on_complete.trim(),
            truncated_result,
            if allow_followup_dispatch {
                "或继续派发下一步节点任务"
            } else {
                "（已达自动回调深度上限，不可再派发后续节点任务）"
            }
        );
        let suffix = "\n\n=== 当前运行模式：异步任务回流自动验收 ===\n\
            - 这是节点完成任务后的唯一面向用户的汇总回执，用户可能不在屏幕前；**像靠谱的人类助理一样自然汇报（3～5 句、≤6 行以内）**。\n\
            - 默认在已有授权范围内直接执行并给结论，不反问。仅当同时存在至少两个可续接的活跃线程或节点回执，且无法唯一判断用户指的是哪一个时，第一句用一句话反问确认（例如「你是指刚才的 A 还是 B？」），当轮禁止调用 `dispatch_node` 或其他变更类工具。普通派发或信息不足但可合理默认时，按默认规则执行。\n\
            - 直接说清核心结果与关键事实；如有具体依据（`file:line`、commit、任务 ID、附件或链接）文末顺手带一句，无则省略。\n\
            - **禁止套用「待审批：」「结论：」「来源：」这类表单前缀**，严禁堆砌多级标题或大表格，严禁复述节点原始全文，不要寒暄。";
        (
            instr,
            suffix,
            "task_callback",
            format!("**[自动跟进 · {task_title}]**"),
            format!("任务自动跟进：{task_title}"),
        )
    } else {
        let instr = format!(
            "{}\n任务「{}」（id=`{}`，执行节点：{}，状态：{}）已完成。\n\n\
             【节点执行结果】\n{}\n\n\
             请将上述节点执行结果提炼成给用户看的简短聊天回复（**3～5 句、≤6 行以内，尽量 ≤200 字**）：\n\
             1. 若需要用户拍板/审批/二选一，第一句直接点明决策项；否则直接给核心结论与 1～3 个关键事实。\n\
             2. 文末如有具体 `file:line`、commit、URL 或附件顺手带一句，没有就别凑。\n\
             3. 节点完整原始报告已存入右侧抽屉「查看结果」，切勿照搬排障过程、逐文件清单、命令输出、大表格、多级标题，也不要套用「待审批：」「结论：」「来源：」等表单标签。",
            crate::llm::TASK_DIGEST_MARKER,
            task_title,
            task_id,
            node_name,
            task_status,
            truncated_result,
        );
        let suffix = "\n\n=== 当前运行模式：节点结果精简提炼 ===\n\
            - 你的唯一任务是把执行节点返回的原始技术报告提炼为自然、干练的中文对话汇报（**3～5 句、≤6 行、尽量 ≤200 字**）。\n\
            - 禁止调用任何工具，也不要输出 JSON 或多级 `#` 标题 / Markdown 大表格 / 大段代码块 / 「结论：」「来源：」前缀。\n\
            - 保留结论、必要决策点与最关键依据；冗长细节已在抽屉「查看结果」留存，无需复述。";
        (
            instr,
            suffix,
            "task_digest",
            format!("**[节点 {node_name} · {task_title}]**"),
            format!("任务完成：{task_title}"),
        )
    };

    let uid_snap = user_id.clone();
    let instr_snap = instruction.clone();
    let secrets = hub.secrets.clone();
    let Ok(snap) = hub
        .db
        .read(move |conn| build_snapshot_sync(conn, &secrets, &uid_snap, &instr_snap, false, None))
        .await
    else {
        return;
    };

    let mode = AgentMode {
        prompt_suffix,
        interactive: false,
        allowed_tools: Some(allowed_tools),
        callback_depth: next_depth,
        initial_thread_id: task_thread_id.clone(),
    };

    match agent_loop(&hub, &user_id, &instruction, &snap, &mode).await {
        Ok(out) => {
            let mut merged_reply = append_missing_attachments(out.reply.trim(), &result_content);
            if let Some(ref footer) = approval_footer {
                if !merged_reply.is_empty() {
                    merged_reply.push_str("\n\n");
                }
                merged_reply.push_str(footer);
            }
            let reply = truncate_bytes(&merged_reply, 16_000);
            let msg = format!("{msg_header}\n\n{reply}");
            let thread_db = out.thread_id.or(task_thread_id);
            let uid_db = user_id.clone();
            let title_db = task_title.clone();
            let audit_detail = truncate_bytes(&reply, 240);
            let _ = hub
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute(
                        "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                         VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                        params![uid_db, thread_db, truncate_bytes(&msg, 16_000), now_unix()],
                    )?;
                    record_audit(&tx, &uid_db, audit_action, "ok", &title_db, &audit_detail)?;
                    tx.commit()?;
                    Ok(())
                })
                .await;
            hub.events.emit(&user_id, "stage", json!({"stage": ""}));
            hub.events.emit(
                &user_id,
                "message",
                json!({"task_id": task_id, "callback": true}),
            );
            let im_notify_title = if task_status == "failed" {
                format!("任务受阻：{task_title}")
            } else {
                im_title
            };
            crate::integrations::im::maybe_notify(
                &hub,
                &user_id,
                "delivery",
                &im_notify_title,
                &reply,
            )
            .await;
        }
        Err(err) => {
            let uid_db = user_id.clone();
            let title_db = task_title.clone();
            let detail = err.1.to_owned();
            let mut raw_body = truncate_bytes(result_content.trim(), 15_000);
            if let Some(ref footer) = approval_footer {
                if !raw_body.is_empty() {
                    raw_body.push_str("\n\n");
                }
                raw_body.push_str(footer);
            }
            let fallback_body = format!("**[节点 {node_name} · {task_title}]**\n\n{raw_body}");
            let thread_db = task_thread_id.clone();
            let _ = hub
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    tx.execute(
                        "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                         VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                        params![
                            uid_db,
                            thread_db,
                            truncate_bytes(&fallback_body, 16_000),
                            now_unix()
                        ],
                    )?;
                    record_audit(&tx, &uid_db, audit_action, "failed", &title_db, &detail)?;
                    tx.commit()?;
                    Ok(())
                })
                .await;
            hub.events.emit(&user_id, "stage", json!({"stage": ""}));
            hub.events.emit(
                &user_id,
                "message",
                json!({"task_id": task_id, "callback_failed": true}),
            );
            let im_notify_title = if task_status == "failed" {
                format!("任务受阻：{task_title}")
            } else {
                format!("任务完成：{task_title}")
            };
            let im_body = match approval_footer {
                Some(ref footer) => format!("{result_content}\n\n{footer}"),
                None => result_content,
            };
            crate::integrations::im::maybe_notify(
                &hub,
                &user_id,
                "delivery",
                &im_notify_title,
                &im_body,
            )
            .await;
        }
    }
}

struct DeltaBatcher<'a> {
    hub: &'a Hub,
    user_id: &'a str,
    enabled: bool,
    buf: String,
    last_flush: Instant,
}

impl<'a> DeltaBatcher<'a> {
    fn new(hub: &'a Hub, user_id: &'a str, enabled: bool) -> Self {
        Self {
            hub,
            user_id,
            enabled,
            buf: String::new(),
            last_flush: Instant::now(),
        }
    }

    fn push(&mut self, piece: &str) {
        if !self.enabled || piece.is_empty() {
            return;
        }
        self.buf.push_str(piece);
        if self.buf.len() >= 24 || self.last_flush.elapsed() >= Duration::from_millis(40) {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if !self.enabled || self.buf.is_empty() {
            self.buf.clear();
            return;
        }
        let chunk = std::mem::take(&mut self.buf);
        self.hub
            .events
            .emit(self.user_id, "delta", json!({"text": chunk}));
        self.last_flush = Instant::now();
    }
}

fn build_openai_messages(
    system_prompt: &str,
    history: &[ChatMessage],
    user_message: &str,
    images: &[crate::llm::ImageInput],
) -> Vec<Value> {
    let mut messages = Vec::with_capacity(history.len() + 2);
    messages.push(json!({"role": "system", "content": system_prompt}));
    for m in history {
        let role = if m.role == "user" {
            "user"
        } else {
            "assistant"
        };
        messages.push(json!({"role": role, "content": m.content}));
    }
    if images.is_empty() {
        messages.push(json!({"role": "user", "content": user_message}));
    } else {
        let mut parts = vec![json!({"type": "text", "text": user_message})];
        for img in images {
            parts.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:{};base64,{}", img.mime_type, img.data_base64)}
            }));
        }
        messages.push(json!({"role": "user", "content": parts}));
    }
    messages
}

/// Primary agent loop using OpenAI native tool calling + SSE streaming.
async fn agent_loop(
    hub: &Hub,
    user_id: &str,
    message: &str,
    snap: &TurnSnapshot,
    mode: &AgentMode<'_>,
) -> Result<AgentOutput> {
    let mut system_prompt = build_system_prompt(
        &snap.profile,
        &snap.rolling_summary,
        &snap.roles,
        &snap.threads,
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
        active_thread_id: mode.initial_thread_id.clone(),
        dispatched_tasks: Vec::new(),
        approval_prompts: Vec::new(),
        done_keys: HashSet::new(),
        interactive: mode.interactive,
        allowed_tools: mode.allowed_tools,
        callback_depth: mode.callback_depth,
    };

    let models = crate::models::resolve(hub, user_id).await?;
    let prepared = prepare_images(&ctx, &models, message).await;
    let prompt_msg = if prepared.context.is_empty() {
        message.to_owned()
    } else {
        format!("{message}\n\n{}", prepared.context)
    };

    let tools_schema = tool_definitions(mode.allowed_tools);
    let mut messages =
        build_openai_messages(&system_prompt, &snap.history, &prompt_msg, &prepared.inline);

    let mut final_reply = String::new();
    let mut unresolved: Vec<ToolOutcome> = Vec::new();
    let mut called_any_tool = false;
    let mut last_tools: Vec<parser::ToolCall> = Vec::new();

    for step in 0..MAX_AGENT_STEPS {
        if step > 0 && mode.interactive {
            hub.events.emit(user_id, "delta", json!({"reset": true}));
        }
        let mut batcher = DeltaBatcher::new(hub, user_id, mode.interactive);
        let resp = match models
            .brain
            .complete_turn_stream(&messages, &tools_schema, mode.interactive, |piece| {
                batcher.push(piece);
            })
            .await
        {
            Ok(r) => {
                batcher.flush();
                r
            }
            Err(e) if step == 0 => return Err(e),
            Err(e) => {
                eprintln!(
                    "[brain] followup step={step} failed user={user_id}: {}",
                    e.1
                );
                batcher.flush();
                break;
            }
        };

        let mut parsed_calls = native_calls_to_tool_calls(&resp.tool_calls);
        let used_native = !parsed_calls.is_empty();
        if used_native {
            final_reply = resp.content.clone();
        } else if resp.content.contains("<tool_call>") || resp.content.contains("<function_call>") {
            let (recovered_reply, recovered_tools) = recover_inline_tool_calls(&resp.content);
            final_reply = recovered_reply;
            parsed_calls = recovered_tools;
        } else {
            final_reply = resp.content.clone();
        }

        if parsed_calls.is_empty() {
            unresolved.clear();
            break;
        }
        called_any_tool = true;
        let mut outcomes = Vec::with_capacity(parsed_calls.len());
        for call in &parsed_calls {
            outcomes.push(execute_tool(&mut ctx, call).await);
        }

        let needs_followup = outcomes.iter().any(|o| o.needs_followup);
        let empty_reply = final_reply.trim().is_empty();
        last_tools = parsed_calls;

        if !needs_followup && !empty_reply {
            unresolved.clear();
            break;
        }
        let last_step = step + 1 == MAX_AGENT_STEPS;
        if !last_step {
            ctx.stage("正在根据工具结果整理回复…");
            if used_native && resp.tool_calls.len() == outcomes.len() {
                let tc_json: Vec<Value> = resp
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": if tc.arguments.trim().is_empty() { "{}" } else { tc.arguments.as_str() },
                            }
                        })
                    })
                    .collect();
                messages.push(json!({
                    "role": "assistant",
                    "content": if resp.content.trim().is_empty() {
                        Value::Null
                    } else {
                        Value::String(truncate_bytes(&resp.content, 4000))
                    },
                    "tool_calls": tc_json,
                }));
                for (tc, o) in resp.tool_calls.iter().zip(outcomes.iter()) {
                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": tc.id,
                        "content": format!(
                            "[{}] {}：{}",
                            o.tool,
                            if o.ok { "成功" } else { "失败" },
                            truncate_bytes(o.observation.trim(), TOOL_OBSERVATION_MAX_BYTES)
                        ),
                    }));
                }
            } else {
                messages.push(json!({
                    "role": "assistant",
                    "content": truncate_bytes(resp.content.trim(), 4000),
                }));
                messages.push(json!({
                    "role": "user",
                    "content": format_tool_results(&outcomes),
                }));
            }
        }
        unresolved = outcomes;
    }

    if final_reply.trim().is_empty() && called_any_tool {
        final_reply = default_reply_for_tools(&last_tools);
    }
    for o in unresolved.iter().filter(|o| o.needs_followup) {
        if !o.user_note.trim().is_empty() {
            if !final_reply.trim().is_empty() {
                final_reply.push_str("\n\n");
            }
            final_reply.push_str(o.user_note.trim());
        }
    }
    for (code, note) in &ctx.approval_prompts {
        let needle = format!("/APPROVE {}", code.to_ascii_uppercase());
        if !final_reply.to_ascii_uppercase().contains(&needle) {
            if !final_reply.trim().is_empty() {
                final_reply.push_str("\n\n");
            }
            final_reply.push_str(note);
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
#[allow(clippy::too_many_arguments)]
async fn persist_turn(
    hub: &Hub,
    user_id: &str,
    message: &str,
    user_msg_id: i64,
    request_id: Option<&str>,
    thread_id: Option<String>,
    final_reply: &str,
    user_context: &str,
) -> Result<String> {
    let uid = user_id.to_owned();
    let tid_ev = thread_id.clone();
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

    hub.events.emit(user_id, "delta", json!({"done": true}));
    hub.events.emit(user_id, "stage", json!({"stage": ""}));
    hub.events.emit(
        user_id,
        "message",
        json!({
            "done": true,
            "message_id": user_msg_id,
            "request_id": request_id,
            "thread_id": tid_ev,
        }),
    );
    Ok(reply_ret)
}

fn format_tool_results(outcomes: &[ToolOutcome]) -> String {
    let mut out =
        String::from("以下是你上一步调用工具的执行结果（由系统自动回传，不是用户发言）：\n");
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
         如确有必要可继续调用工具，但不要重复已成功的调用。",
    );
    out
}

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

#[cfg(test)]
mod tests {
    use super::should_auto_digest;

    #[test]
    fn should_auto_digest_only_triggers_on_verbose_or_structured_output() {
        assert!(!should_auto_digest("已重启服务，健康检查通过。"));
        assert!(!should_auto_digest(
            "第一行结论\n第二行关键事实\n\n![ev](/api/attachments/att_0123456789abcdef0123456789abcdef)"
        ));
        assert!(should_auto_digest("1\n2\n3\n4\n5"));
        assert!(should_auto_digest(&"a".repeat(261)));
        assert!(should_auto_digest("总结如下：\n```sh\necho hi\n```"));
        assert!(should_auto_digest("## 详细排查报告\n一切正常"));
    }
}
