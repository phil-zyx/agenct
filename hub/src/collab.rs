//! Lightweight cross-person collaboration: @colleague delegation & manual accept/deliver.
use crate::{
    attachments,
    auth::{sha256_hex, AuthUser},
    automation,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, nodes, Hub, Result,
};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

/// Delegates a lightweight collaborative task from requester to an active peer.
/// Honors peer's `auto_accept` permission (default false): if disabled, waits for peer manual action.
pub async fn delegate_to_peer(
    hub: &Hub,
    requester_id: &str,
    peer_query: &str,
    instruction: &str,
) -> Result<(String, String, &'static str)> {
    let req_id = requester_id.to_owned();
    let needle = peer_query.trim().to_ascii_lowercase();
    let instr = truncate_bytes(instruction.trim(), 4000);
    if needle.is_empty() || instr.is_empty() {
        return Err(invalid("peer and instruction required"));
    }

    let (peer_id, peer_name, task_id, mode, queued_node_id) = hub
        .db
        .write(move |conn| {
            let peer: Option<(String, String)> = conn
                .query_row(
                    "SELECT id, display_name FROM users
                     WHERE status='active' AND id != ?1
                       AND (lower(username) = ?2 OR lower(display_name) = ?2 OR lower(id) = ?2)
                     LIMIT 1",
                    params![req_id, needle],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((peer_id, peer_name)) = peer else {
                return Err(invalid("colleague not found or inactive"));
            };

            let req_name: String = conn.query_row(
                "SELECT display_name FROM users WHERE id = ?1",
                [&req_id],
                |r| r.get(0),
            )?;
            let perms = automation::read_permissions_sync(conn, &peer_id)?;
            let now = now_unix();
            let title = format!("来自 {req_name} 的协作：{}", truncate_bytes(&instr, 48));

            let (task_id, mode, queued_node_id) = if perms.auto_accept {
                if let Some(node) =
                    nodes::pick_node_for_task_sync(conn, &peer_id, None, false, Some(&instr))?
                {
                    let (tid, _) = nodes::create_task_and_enqueue_sync(
                        conn,
                        &req_id,
                        &peer_id,
                        &node.id,
                        &title,
                        &instr,
                        "delegation",
                    )?;
                    (tid, "auto_queued", Some(node.id))
                } else {
                    let tid = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
                    conn.execute(
                        "INSERT INTO tasks(id, requester_id, owner_id, title, instruction, status, revision, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6)",
                        params![tid, req_id, peer_id, title, instr, now],
                    )?;
                    (tid, "pending_no_node", None)
                }
            } else {
                let tid = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
                conn.execute(
                    "INSERT INTO tasks(id, requester_id, owner_id, title, instruction, status, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6)",
                    params![tid, req_id, peer_id, title, instr, now],
                )?;
                (tid, "pending_manual", None)
            };

            let peer_notice = format!("**[来自 {req_name} 的协作请求]**\n\n{instr}");
            conn.execute(
                "INSERT INTO messages(user_id, peer_id, role, content, status, created_at)
                 VALUES (?1, ?2, 'system', ?3, 'complete', ?4)",
                params![peer_id, req_id, peer_notice, now],
            )?;
            record_audit(
                conn,
                &req_id,
                "delegate",
                mode,
                &format!("Delegated to {peer_name}"),
                &truncate_bytes(&instr, 200),
            )?;
            Ok((peer_id, peer_name, task_id, mode, queued_node_id))
        })
        .await?;

    if let Some(nid) = queued_node_id {
        hub.node_waiters.notify(&peer_id, &nid);
    }
    attachments::grant_in_text(hub, requester_id, &peer_id, instruction).await?;

    hub.events.emit(
        peer_id.clone(),
        "task",
        json!({"task_id": task_id, "mode": mode}),
    );
    hub.events
        .emit(peer_id.clone(), "message", json!({"from": requester_id}));
    hub.events
        .emit(requester_id.to_owned(), "task", json!({"task_id": task_id}));

    let h_bg = hub.clone();
    let p_bg = peer_id.clone();
    let instr_bg = instruction.to_owned();
    tokio::spawn(async move {
        crate::integrations::im::maybe_notify(
            &h_bg,
            &p_bg,
            "delegation",
            "收到新的同事协作请求",
            &instr_bg,
        )
        .await;
    });

    Ok((peer_name, task_id, mode))
}

pub const ELEVATE_APPROVAL_TTL_SECS: i64 = 600;

pub struct ElevatedTaskOutcome {
    pub task_id: String,
    pub node_id: String,
    pub node_name: String,
    pub title: String,
    pub thread_id: Option<String>,
}

pub fn approval_code_hash(user_id: &str, code: &str) -> String {
    sha256_hex(&format!("{user_id}:{}", code.trim().to_ascii_uppercase()))
}

fn elevate_task_in_tx(
    tx: &rusqlite::Transaction<'_>,
    uid: &str,
    tid: &str,
    wanted_node: Option<&str>,
    source: &str,
    now: i64,
) -> Result<ElevatedTaskOutcome> {
    struct RetryTaskRow {
        requester_id: String,
        status: String,
        revision: i64,
        title: String,
        instruction: String,
        thread_id: Option<String>,
        existing_node_id: Option<String>,
        existing_agent: String,
        existing_role: String,
        existing_model: String,
        existing_effort: String,
    }
    let task: Option<RetryTaskRow> = tx
        .query_row(
            "SELECT t.requester_id, t.status, t.revision, t.title, t.instruction, t.thread_id,
                    j.node_id, COALESCE(j.agent, ''), COALESCE(j.role, ''),
                    COALESCE(j.model, ''), COALESCE(j.effort, '')
             FROM tasks t
             LEFT JOIN node_jobs j ON j.task_id = t.id
             WHERE t.id = ?1 AND t.owner_id = ?2",
            rusqlite::params![tid, uid],
            |r| {
                Ok(RetryTaskRow {
                    requester_id: r.get(0)?,
                    status: r.get(1)?,
                    revision: r.get(2)?,
                    title: r.get(3)?,
                    instruction: r.get(4)?,
                    thread_id: r.get(5)?,
                    existing_node_id: r.get(6)?,
                    existing_agent: r.get(7)?,
                    existing_role: r.get(8)?,
                    existing_model: r.get(9)?,
                    existing_effort: r.get(10)?,
                })
            },
        )
        .optional()?;
    let Some(row) = task else {
        return Err(denied());
    };
    if row.requester_id != uid {
        return Err(invalid("cross-user delegated tasks cannot be elevated"));
    }
    if !matches!(row.status.as_str(), "failed" | "done" | "pending") {
        return Err(invalid(
            "running or canceled tasks cannot use elevated retry",
        ));
    }
    let target_node = wanted_node.or(row.existing_node_id.as_deref());
    let node = nodes::pick_node_for_task_sync(tx, uid, target_node, false, Some(&row.instruction))?
        .ok_or_else(|| invalid("no registered node available"))?;
    let supports_elevated = node
        .capabilities
        .split('|')
        .any(|seg| seg.trim().eq_ignore_ascii_case("elevated:1"));
    if !supports_elevated {
        return Err(invalid(
            "目标节点未开启单次放行支持（请在节点启动参数中加入 --allow-elevated）",
        ));
    }
    let agent = if row.existing_agent.trim().is_empty() {
        node.default_agent.clone()
    } else {
        row.existing_agent
    };
    let next_rev = row.revision + 1;
    tx.execute(
        "UPDATE tasks
         SET status = 'running', blocked_reason = '', revision = ?2, updated_at = ?3
         WHERE id = ?1",
        params![tid, next_rev, now],
    )?;
    tx.execute(
        "INSERT INTO node_jobs(
            task_id, user_id, node_id, agent, role, model, effort, sandbox,
            task_revision, authorization, status, updated_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'elevated', ?8, 'manual', 'queued', ?9)
         ON CONFLICT(task_id) DO UPDATE SET
           node_id = excluded.node_id,
           agent = excluded.agent,
           role = excluded.role,
           model = excluded.model,
           effort = excluded.effort,
           sandbox = 'elevated',
           task_revision = excluded.task_revision,
           authorization = 'manual',
           status = 'queued',
           lease_hash = NULL,
           lease_until = NULL,
           submission_hash = NULL,
           stage = 'Queued (elevated)',
           updated_at = excluded.updated_at",
        params![
            tid,
            uid,
            node.id,
            agent,
            row.existing_role,
            row.existing_model,
            row.existing_effort,
            next_rev,
            now
        ],
    )?;
    tx.execute(
        "UPDATE elevate_approvals SET used_at = ?2 WHERE task_id = ?1 AND used_at IS NULL",
        params![tid, now],
    )?;
    record_audit(
        tx,
        uid,
        "task_elevate",
        "queued",
        &row.title,
        &format!("node={} 单次放行重试 ({source})", node.name),
    )?;
    Ok(ElevatedTaskOutcome {
        task_id: tid.to_owned(),
        node_id: node.id,
        node_name: node.name,
        title: row.title,
        thread_id: row.thread_id,
    })
}

pub async fn elevate_task_once(
    hub: &Hub,
    user_id: &str,
    task_id: &str,
    wanted_node: Option<&str>,
    source: &str,
) -> Result<ElevatedTaskOutcome> {
    let uid = user_id.to_owned();
    let tid = task_id.to_owned();
    let wanted = wanted_node.map(str::to_owned);
    let src = source.to_owned();
    let outcome = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = now_unix();
            let outcome = elevate_task_in_tx(&tx, &uid, &tid, wanted.as_deref(), &src, now)?;
            tx.commit()?;
            Ok(outcome)
        })
        .await?;
    hub.node_waiters.notify(user_id, &outcome.node_id);
    hub.events.emit(
        user_id,
        "task",
        json!({
            "task_id": outcome.task_id,
            "status": "running",
            "node_id": outcome.node_id,
            "elevated": true,
            "title": outcome.title,
            "node_name": outcome.node_name,
        }),
    );
    Ok(outcome)
}

const APPROVAL_LIMIT_WINDOW: Duration = Duration::from_secs(600);
const APPROVAL_MAX_FAILURES: usize = 5;

/// Per-user sliding-window rate limiter protecting `/approve <code>` against brute-force guessing.
#[derive(Clone, Default)]
pub struct ApprovalLimiter {
    failures: Arc<Mutex<HashMap<String, VecDeque<Instant>>>>,
}

impl ApprovalLimiter {
    fn prune(q: &mut VecDeque<Instant>, now: Instant) {
        while q
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= APPROVAL_LIMIT_WINDOW)
        {
            q.pop_front();
        }
    }

    pub fn check_allowed(&self, user_id: &str) -> bool {
        let mut map = self.failures.lock().expect("approval limiter mutex");
        let now = Instant::now();
        map.retain(|_, q| {
            Self::prune(q, now);
            !q.is_empty()
        });
        map.get(user_id)
            .is_none_or(|q| q.len() < APPROVAL_MAX_FAILURES)
    }

    pub fn record_failure(&self, user_id: &str) {
        let mut map = self.failures.lock().expect("approval limiter mutex");
        let now = Instant::now();
        let q = map.entry(user_id.to_owned()).or_default();
        Self::prune(q, now);
        q.push_back(now);
    }

    pub fn reset(&self, user_id: &str) {
        self.failures
            .lock()
            .expect("approval limiter mutex")
            .remove(user_id);
    }
}

pub async fn consume_approval_code(
    hub: &Hub,
    user_id: &str,
    raw_code: &str,
) -> Result<ElevatedTaskOutcome> {
    if !hub.approval_limiter.check_allowed(user_id) {
        return Err(crate::Error(
            StatusCode::TOO_MANY_REQUESTS,
            "审批码错误次数过多，请稍后再试",
        ));
    }
    let code = raw_code.trim().to_ascii_uppercase();
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        hub.approval_limiter.record_failure(user_id);
        let uid = user_id.to_owned();
        let _ = hub
            .db
            .write(move |conn| {
                record_audit(
                    conn,
                    &uid,
                    "task_elevate_denied",
                    "failed",
                    "单次放行审批码校验失败",
                    "invalid_code_format",
                )
            })
            .await;
        return Err(invalid("审批码格式无效，应为 6 位字母或数字"));
    }
    let code_hash = approval_code_hash(user_id, &code);
    let uid = user_id.to_owned();
    let res = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = now_unix();
            let tid: Option<String> = tx
                .query_row(
                    "SELECT task_id FROM elevate_approvals
                     WHERE user_id = ?1 AND code_hash = ?2 AND used_at IS NULL AND expires_at >= ?3
                     ORDER BY created_at DESC LIMIT 1",
                    params![uid, code_hash, now],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(tid) = tid else {
                record_audit(
                    &tx,
                    &uid,
                    "task_elevate_denied",
                    "failed",
                    "单次放行审批码校验失败",
                    "invalid_or_expired_code",
                )?;
                tx.commit()?;
                return Ok(None);
            };
            let outcome = elevate_task_in_tx(&tx, &uid, &tid, None, "chat_approve", now)?;
            tx.commit()?;
            Ok(Some(outcome))
        })
        .await?;
    let Some(outcome) = res else {
        hub.approval_limiter.record_failure(user_id);
        return Err(invalid("审批码无效、已使用或已过期"));
    };
    hub.approval_limiter.reset(user_id);
    hub.node_waiters.notify(user_id, &outcome.node_id);
    hub.events.emit(
        user_id,
        "task",
        json!({
            "task_id": outcome.task_id,
            "status": "running",
            "node_id": outcome.node_id,
            "elevated": true,
            "title": outcome.title,
            "node_name": outcome.node_name,
        }),
    );
    Ok(outcome)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskActionInput {
    pub action: String, // "assign_node" | "retry_elevated" | "deliver" | "cancel"
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

/// Unified lightweight task action endpoint (assign to node / manually deliver / cancel).
pub async fn task_action(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Path(task_id): Path<String>,
    Json(input): Json<TaskActionInput>,
) -> Result<Json<Value>> {
    let uid = user.id.clone();
    let tid = task_id.clone();

    match input.action.as_str() {
        "cancel" => nodes::cancel_task(&hub, &user, &task_id).await,
        "assign_node" => {
            let wanted_node = input.node_id.clone();
            let node_id = hub
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let task: Option<(String, i64, String)> = tx
                        .query_row(
                            "SELECT status, revision, instruction FROM tasks WHERE id = ?1 AND owner_id = ?2",
                            params![tid, uid],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let Some((status, rev, instruction)) = task else {
                        return Err(denied());
                    };
                    if !matches!(status.as_str(), "pending" | "failed") {
                        return Err(invalid("task cannot be assigned in current status"));
                    }
                    let node = nodes::pick_node_for_task_sync(
                        &tx,
                        &uid,
                        wanted_node.as_deref(),
                        false,
                        Some(&instruction),
                    )?
                    .ok_or_else(|| invalid("no registered node available"))?;
                    let now = now_unix();
                    let next_rev = rev + 1;
                    tx.execute(
                        "UPDATE tasks SET status='running', blocked_reason='', revision=?2, updated_at=?3 WHERE id=?1",
                        params![tid, next_rev, now],
                    )?;
                    tx.execute(
                        "INSERT INTO node_jobs(task_id, user_id, node_id, agent, task_revision, authorization, status, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'manual', 'queued', ?6)
                         ON CONFLICT(task_id) DO UPDATE SET
                           node_id=excluded.node_id, agent=excluded.agent, sandbox='', task_revision=excluded.task_revision,
                           authorization='manual', status='queued',
                           lease_hash=NULL, lease_until=NULL, submission_hash=NULL,
                           stage='Queued', updated_at=excluded.updated_at",
                        params![tid, uid, node.id, node.default_agent, next_rev, now],
                    )?;
                    tx.commit()?;
                    Ok(node.id)
                })
                .await?;
            hub.node_waiters.notify(&user.id, &node_id);
            hub.events.emit(
                user.id,
                "task",
                json!({"task_id": task_id, "status": "running", "node_id": node_id}),
            );
            Ok(Json(
                json!({"ok": true, "task_id": task_id, "node_id": node_id}),
            ))
        }
        "retry_elevated" => {
            let outcome =
                elevate_task_once(&hub, &user.id, &task_id, input.node_id.as_deref(), "web")
                    .await?;
            Ok(Json(json!({
                "ok": true,
                "task_id": outcome.task_id,
                "node_id": outcome.node_id,
                "sandbox": "elevated",
            })))
        }
        "deliver" => {
            let body = input
                .result
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| invalid("delivery result cannot be empty"))?;
            let clean = truncate_bytes(body, 16_000);
            let clean_db = clean.clone();
            let requester_id = hub
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let row: Option<(String, String, String)> = tx
                        .query_row(
                            "SELECT requester_id, title, status FROM tasks WHERE id = ?1 AND owner_id = ?2",
                            params![tid, uid],
                            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                        )
                        .optional()?;
                    let Some((req_id, title, status)) = row else {
                        return Err(denied());
                    };
                    if status == "canceled" {
                        return Err(invalid("canceled task cannot be delivered"));
                    }
                    let now = now_unix();
                    tx.execute(
                        "UPDATE tasks SET status='done', result=?2, revision=revision+1, updated_at=?3
                         WHERE id=?1",
                        params![tid, clean_db, now],
                    )?;
                    tx.execute(
                        "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?2
                         WHERE task_id=?1 AND status IN ('queued','running')",
                        params![tid, now],
                    )?;
                    if req_id != uid {
                        let msg = format!("**[协作交付 · {title}]**\n\n{clean_db}");
                        tx.execute(
                            "INSERT INTO messages(user_id, peer_id, role, content, status, created_at)
                             VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                            params![req_id, uid, msg, now],
                        )?;
                    }
                    record_audit(&tx, &uid, "task_deliver", "ok", &title, &truncate_bytes(&clean_db, 200))?;
                    tx.commit()?;
                    Ok(req_id)
                })
                .await?;
            if requester_id != user.id {
                attachments::grant_in_text(&hub, &user.id, &requester_id, &clean).await?;
                hub.events
                    .emit(requester_id, "message", json!({"task_id": task_id}));
            }
            hub.events.emit(
                user.id,
                "task",
                json!({"task_id": task_id, "status": "done"}),
            );
            Ok(Json(
                json!({"ok": true, "task_id": task_id, "status": "done"}),
            ))
        }
        _ => Err(invalid(
            "action must be assign_node, retry_elevated, deliver, or cancel",
        )),
    }
}
