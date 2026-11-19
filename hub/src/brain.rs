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
const DEFAULT_SOUL_MD: &str = "# SOUL.md\n- 身份：用户的 7×24h 个人超级智能体 Agenct。\n- 风格：干练、直接、结论先行，用第一人称自然沟通。\n- 原则：云端能直接解决的直接解决；需要操作代码/终端/本机环境时调度在线节点；跨人承诺或高危动作主动说明边界。";
const DEFAULT_MEMORY_MD: &str = "# MEMORY.md\n- 在这里记录长期偏好、工作环境事实与沟通习惯。";

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

    hub.events.emit(
        user_id.to_owned(),
        "stage",
        json!({"stage": "正在整理上下文与记忆…"}),
    );

    // Persist user message + load structured workbench snapshot cleanly in one DB read/write.
    let uid = user_id.to_owned();
    let msg_owned = message.to_owned();
    let rid_owned = request_id.map(str::to_owned);
    let snapshot = hub
        .db
        .write(move |conn| {
            let profile = read_profile_sync(conn, &uid)?;
            let ledgers = list_ledgers_sync(conn, &uid)?;
            let nodes = nodes::list_nodes_sync(conn, &uid)?;
            let crons = automation::list_crons_sync(conn, &uid)?;
            let rules = automation::list_rules_sync(conn, &uid)?;
            let matched_rule = automation::match_rule_sync(conn, &uid, &msg_owned)?;

            let mut hist_stmt = conn.prepare(
                "SELECT role, content FROM (
                    SELECT id, role, content FROM messages
                    WHERE user_id = ?1 AND status = 'complete' AND role IN ('user','assistant')
                    ORDER BY id DESC LIMIT 12
                 ) ORDER BY id ASC",
            )?;
            let history: Vec<ChatMessage> = hist_stmt
                .query_map([&uid], |r| {
                    Ok(ChatMessage {
                        role: r.get(0)?,
                        content: truncate_bytes(&r.get::<_, String>(1)?, 1200),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;

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

            Ok((profile, ledgers, nodes, crons, rules, matched_rule, history, contacts))
        })
        .await?;

    let (profile, ledgers, node_list, crons, rules, matched_rule, history, contacts) = snapshot;

    // Build structured system prompt with per-section budgets (system rules & tools are NEVER truncated).
    let system_prompt =
        build_system_prompt(&profile, &ledgers, &node_list, &crons, &rules, &contacts);

    hub.events.emit(
        user_id.to_owned(),
        "stage",
        json!({"stage": "正在思考与决策…"}),
    );

    let raw_llm = hub
        .llm
        .complete(&system_prompt, &history, message, true)
        .await?;

    let parsed = parse_brain_response(&raw_llm);
    let mut final_reply = parsed.reply;
    let mut dispatched_tasks: Vec<Value> = Vec::new();

    for call in parsed.tools {
        match call.tool.as_str() {
            "update_memory" => {
                apply_update_memory(hub, user_id, &call).await?;
            }
            "update_ledger" => {
                apply_update_ledger(hub, user_id, &call).await?;
            }
            "manage_cron" => {
                apply_manage_cron(hub, user_id, &call).await?;
            }
            "manage_rule" => {
                apply_manage_rule(hub, user_id, &call).await?;
            }
            "fetch_gitlab_commits" => {
                if let Some(summary) =
                    crate::integrations::gitlab::fetch_commit_summary(hub, user_id).await
                {
                    final_reply.push_str("\n\n");
                    final_reply.push_str(&summary);
                }
            }
            "dispatch_node" => {
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
                if let Some(info) = dispatch_to_node(
                    hub,
                    user_id,
                    preferred_node.as_deref(),
                    prefer_cloud,
                    &title,
                    &instr,
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

    // Persist assistant reply.
    let uid = user_id.to_owned();
    let reply_db = truncate_bytes(final_reply.trim(), 16_000);
    let reply_ret = reply_db.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO messages(user_id, role, content, status, created_at)
                 VALUES (?1, 'assistant', ?2, 'complete', ?3)",
                params![uid, reply_db, now_unix()],
            )?;
            Ok(())
        })
        .await?;

    hub.events
        .emit(user_id.to_owned(), "stage", json!({"stage": ""}));
    hub.events
        .emit(user_id.to_owned(), "message", json!({"done": true}));

    Ok(json!({
        "reply": reply_ret,
        "tasks": dispatched_tasks,
    }))
}

fn build_system_prompt(
    profile: &AgentProfile,
    ledgers: &[Ledger],
    node_list: &[nodes::NodeView],
    crons: &[automation::CronJob],
    rules: &[automation::DispatchRule],
    contacts: &[String],
) -> String {
    let soul = truncate_bytes(&profile.soul_md, 1500);
    let memory = truncate_bytes(&profile.memory_md, 2000);

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

=== SOUL.md ===
{soul}

=== MEMORY.md ===
{memory}

=== 实时工作台快照 ===
【专项活账本 (ledgers)】
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
{{"reply":"直接展示给用户的自然中文回复","tools":[]}}

=== 可组合调用的工具（tools 数组可为空或多项） ===
1. update_memory: {{"tool":"update_memory","name":null,"soul_md":null,"memory_append":"追加一条长期记忆/规矩","memory_replace":null}}
2. update_ledger: {{"tool":"update_ledger","action":"upsert|mute|done|delete","ledger_id":null,"title":"专项名","status":"active|muted|done","notes_append":"增量记录","notes_replace":null}}
3. manage_cron: {{"tool":"manage_cron","action":"add|delete","cron_id":null,"daily_time_cst":"HH:MM或null","interval_minutes":60,"target":"cloud|auto|node_id","instruction":"完整指令"}}
4. manage_rule: {{"tool":"manage_rule","action":"add|delete","rule_id":null,"keyword":"关键词","node_id":"目标节点或cloud"}}
5. fetch_gitlab_commits: {{"tool":"fetch_gitlab_commits"}}
6. dispatch_node: {{"tool":"dispatch_node","node_id":null,"prefer_cloud":false,"title":"简短标题","instruction":"补全全部上下文指代后的自包含执行指令"}}
   - 涉及本机代码/tmux/私有环境时 prefer_cloud=false；通用云端脚本/分析/爬虫/截图任务可设 prefer_cloud=true。
7. delegate_colleague: {{"tool":"delegate_colleague","peer":"同事username或姓名","instruction":"需要同事协助的清晰事项"}}"#,
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
                if !p.memory_md.contains(&line) {
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

async fn dispatch_to_node(
    hub: &Hub,
    user_id: &str,
    preferred_node: Option<&str>,
    prefer_cloud: bool,
    title: &str,
    instruction: &str,
) -> Result<Option<Value>> {
    let uid = user_id.to_owned();
    let pref = preferred_node.map(str::to_owned);
    let t = title.to_owned();
    let instr = instruction.to_owned();
    let res = hub
        .db
        .write(move |conn| {
            let Some(node) = nodes::pick_node_sync(conn, &uid, pref.as_deref(), prefer_cloud)?
            else {
                return Ok(None);
            };
            let (task_id, _) = nodes::create_task_and_enqueue_sync(
                conn, &uid, &uid, &node.id, &t, &instr, "manual",
            )?;
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
