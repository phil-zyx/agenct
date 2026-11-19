//! Unified Execution Node plane (PC Nodes & Cloud Container Nodes) using Lease Protocol 2.
use crate::{
    attachments,
    auth::{random_token, sha256_hex},
    automation,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, Error, Hub, Result,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const ONLINE_WINDOW_SECS: i64 = 45;
const LEASE_TTL_SECS: i64 = 120;
const MAX_NODES_PER_USER: i64 = 16;

#[derive(Clone, Debug, Serialize)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub capabilities: String,
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

pub fn list_nodes_sync(conn: &Connection, user_id: &str) -> Result<Vec<NodeView>> {
    let now = now_unix();
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, capabilities, last_seen_at
         FROM nodes WHERE user_id = ?1 ORDER BY created_at ASC",
    )?;
    let rows = stmt.query_map([user_id], |r| {
        let last_seen: Option<i64> = r.get(4)?;
        let online = last_seen.is_some_and(|t| now.saturating_sub(t) <= ONLINE_WINDOW_SECS);
        Ok(NodeView {
            id: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            capabilities: r.get(3)?,
            online,
            last_seen_at: last_seen,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Picks the best matching node: explicit node_id -> online preferred kind -> any online -> any registered.
pub fn pick_node_sync(
    conn: &Connection,
    user_id: &str,
    preferred_id: Option<&str>,
    prefer_cloud: bool,
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
    let want_kind = if prefer_cloud { "cloud" } else { "pc" };
    if let Some(n) = nodes.iter().find(|n| n.online && n.kind == want_kind) {
        return Ok(Some(n.clone()));
    }
    if let Some(n) = nodes.iter().find(|n| n.online) {
        return Ok(Some(n.clone()));
    }
    Ok(nodes.into_iter().next())
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
    let now = now_unix();
    let task_id = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
    let clean_title = truncate_bytes(title.trim(), 160);
    let clean_instr = truncate_bytes(instruction.trim(), 8000);
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO tasks(id, requester_id, owner_id, title, instruction, status, revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'running', 1, ?6, ?6)",
        params![task_id, requester_id, owner_id, clean_title, clean_instr, now],
    )?;
    tx.execute(
        "INSERT INTO node_jobs(task_id, user_id, node_id, task_revision, authorization, status, updated_at)
         VALUES (?1, ?2, ?3, 1, ?4, 'queued', ?5)",
        params![task_id, owner_id, node_id, authorization, now],
    )?;
    tx.commit()?;
    Ok((task_id, 1))
}

pub async fn list(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<NodeView>>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id;
    let nodes = hub.db.read(move |conn| list_nodes_sync(conn, &uid)).await?;
    Ok(Json(nodes))
}

pub async fn create(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<CreateNodeInput>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
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
    let token = format!("agn_{}", random_token());
    let token_hash = sha256_hex(&token);
    let uid = user.id.clone();
    let name_owned = name.to_owned();
    let kind = input.kind;
    let caps = truncate_bytes(input.capabilities.trim(), 400);
    let id_clone = req_id.clone();

    let created =
        hub.db
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
                conn.execute(
                "INSERT INTO nodes(id, user_id, name, kind, capabilities, token_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![final_id, uid, name_owned, kind, caps, token_hash, now_unix()],
            )?;
                record_audit(
                    conn,
                    &uid,
                    "node_create",
                    "ok",
                    &format!("Added {kind} node {name_owned}"),
                    &format!("id={final_id}"),
                )?;
                Ok(NodeView {
                    id: final_id,
                    name: name_owned,
                    kind,
                    capabilities: caps,
                    online: false,
                    last_seen_at: None,
                })
            })
            .await?;

    hub.events.emit(
        user.id,
        "node",
        json!({"action": "created", "node": created}),
    );

    let origin = &hub.config.public_origin;
    Ok(Json(json!({
        "node": created,
        "token": token,
        "install_unix": format!("curl -fsSL {origin}/node/install.sh | sh -s -- {token}"),
        "install_ps": format!("irm {origin}/node/install.ps1 | iex; agenct-node --hub {origin} --token {token}"),
    })))
}

pub async fn rotate_token(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let token = format!("agn_{}", random_token());
    let token_hash = sha256_hex(&token);
    let uid = user.id;
    let nid = node_id.clone();

    hub.db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE nodes SET token_hash = ?3 WHERE user_id = ?1 AND id = ?2",
                params![uid, nid, token_hash],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            // Invalidate any running leases held by the rotated token.
            conn.execute(
                "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?3
                 WHERE user_id = ?1 AND node_id = ?2 AND status = 'running'",
                params![uid, nid, now_unix()],
            )?;
            Ok(())
        })
        .await?;

    Ok(Json(json!({"id": node_id, "token": token})))
}

pub async fn remove(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();
    let nid = node_id.clone();
    hub.db
        .write(move |conn| {
            let changed = conn.execute(
                "DELETE FROM nodes WHERE user_id = ?1 AND id = ?2",
                params![uid, nid],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            Ok(())
        })
        .await?;
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
        .filter(|s| !s.is_empty() && s.len() <= 400 && !s.chars().any(char::is_control))
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

pub async fn work(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Value>> {
    let (user_id, node_id, node_name) = authenticate_node(&hub, &headers).await?;
    let uid = user_id.clone();
    let nid = node_id.clone();

    let claimed = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let perms = automation::read_permissions_sync(&tx, &uid)?;
            automation::revoke_disallowed_jobs_sync(&tx, &uid, &perms)?;
            let now = now_unix();
            tx.execute(
                "UPDATE node_jobs SET status='failed', updated_at=?3
                 WHERE user_id=?1 AND node_id=?2 AND status='running'
                   AND (lease_until IS NULL OR lease_until <= ?3)",
                params![uid, nid, now],
            )?;
            let pending: Option<(String, i64, String, String)> = tx
                .query_row(
                    "SELECT j.task_id, j.task_revision, t.title, t.instruction
                     FROM node_jobs j
                     JOIN tasks t ON t.id = j.task_id
                     WHERE j.user_id = ?1 AND j.node_id = ?2
                       AND j.status = 'queued' AND t.status = 'running' AND t.revision = j.task_revision
                     ORDER BY j.updated_at ASC LIMIT 1",
                    params![uid, nid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            let Some((task_id, task_revision, title, instruction)) = pending else {
                tx.commit()?;
                return Ok(None);
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
            Ok(Some((task_id, task_revision, lease_token, lease_until, title, instruction)))
        })
        .await?;

    let Some((task_id, task_revision, lease_token, lease_until, title, instruction)) = claimed
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
        json!({"task_id": task_id, "status": "running", "stage": "Claimed by node", "node_id": node_id}),
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

    let lease_until = hub
        .db
        .write(move |conn| {
            let perms = automation::read_permissions_sync(conn, &uid)?;
            automation::revoke_disallowed_jobs_sync(conn, &uid, &perms)?;
            let now = now_unix();
            let until = now + LEASE_TTL_SECS;
            let changed = conn.execute(
                "UPDATE node_jobs
                 SET lease_until = ?6, stage = ?7, log_tail = ?8, updated_at = ?9
                 WHERE task_id = ?1 AND user_id = ?2 AND node_id = ?3
                   AND lease_hash = ?4 AND task_revision = ?5
                   AND status = 'running' AND lease_until > ?9",
                params![tid, uid, node_id, lease_hash, rev, until, st_clone, tl_clone, now],
            )?;
            if changed != 1 {
                return Err(Error(StatusCode::CONFLICT, "node lease is no longer valid"));
            }
            Ok(until)
        })
        .await?;

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

    // Save any evidence attachments first (owner + task requester get read grants).
    let requester_id: String = {
        let tid = task_id.clone();
        let uid = user_id.clone();
        hub.db
            .read(move |conn| {
                conn.query_row(
                    "SELECT requester_id FROM tasks WHERE id = ?1 AND owner_id = ?2",
                    params![tid, uid],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(denied)
            })
            .await?
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
    let final_status = if input.status == "completed" {
        "done"
    } else {
        "failed"
    };
    let job_status = input.status.clone();
    let content_db = content.clone();
    let rev = input.expected_revision;
    let node_name_db = node_name.clone();

    let req_for_tx = requester_id.clone();
    let (title, is_cross_user, auto_deliver, already_submitted) = hub
        .db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let perms = automation::read_permissions_sync(&tx, &uid)?;
            let now = now_unix();
            struct JobRow {
                status: String,
                lease_hash: Option<String>,
                lease_until: Option<i64>,
                submission_hash: Option<String>,
            }
            let job: Option<JobRow> = tx
                .query_row(
                    "SELECT status, lease_hash, lease_until, submission_hash
                     FROM node_jobs
                     WHERE task_id = ?1 AND user_id = ?2 AND node_id = ?3 AND task_revision = ?4",
                    params![tid, uid, nid, rev],
                    |r| {
                        Ok(JobRow {
                            status: r.get(0)?,
                            lease_hash: r.get(1)?,
                            lease_until: r.get(2)?,
                            submission_hash: r.get(3)?,
                        })
                    },
                )
                .optional()?;
            let Some(JobRow {
                status: cur_status,
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
                return Ok((title, req_for_tx != uid, perms.auto_deliver, true));
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
            tx.execute(
                "UPDATE node_jobs
                 SET status = ?2, submission_hash = ?3, stage = ?4, updated_at = ?5
                 WHERE task_id = ?1",
                params![tid, job_status, submission_hash, final_status, now],
            )?;
            let (title, req_id): (String, String) = tx.query_row(
                "SELECT title, requester_id FROM tasks WHERE id = ?1",
                [&tid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            tx.execute(
                "UPDATE tasks SET status = ?2, result = ?3, revision = revision + 1, updated_at = ?4
                 WHERE id = ?1",
                params![tid, final_status, content_db, now],
            )?;
            // Post result back to owner's chat stream.
            let msg_body = format!("**[节点 {node_name_db} · {title}]**\n\n{content_db}");
            tx.execute(
                "INSERT INTO messages(user_id, role, content, status, created_at)
                 VALUES (?1, 'assistant', ?2, 'complete', ?3)",
                params![uid, msg_body, now],
            )?;
            // If cross-user task and owner opted into auto_deliver, share result to requester too.
            if req_id != uid && perms.auto_deliver && final_status == "done" {
                let peer_msg = format!("**[协作交付 · {title}]**\n\n{content_db}");
                tx.execute(
                    "INSERT INTO messages(user_id, peer_id, role, content, status, created_at)
                     VALUES (?1, ?2, 'assistant', ?3, 'complete', ?4)",
                    params![req_id, uid, peer_msg, now],
                )?;
            }
            record_audit(
                &tx,
                &uid,
                "node_submit",
                final_status,
                &format!("{node_name_db}: {title}"),
                &truncate_bytes(&content_db, 240),
            )?;
            tx.commit()?;
            Ok((title, req_id != uid, perms.auto_deliver, false))
        })
        .await?;

    if !already_submitted {
        hub.events.emit(
            user_id.clone(),
            "task",
            json!({"task_id": task_id, "status": final_status, "result": content}),
        );
        hub.events
            .emit(user_id.clone(), "message", json!({"task_id": task_id}));
        if is_cross_user && auto_deliver && final_status == "done" {
            hub.events
                .emit(requester_id.clone(), "message", json!({"task_id": task_id}));
        }
        // Trigger optional GitLab reply & IM notification asynchronously.
        let h_bg = hub.clone();
        let tid_bg = task_id.clone();
        let uid_bg = user_id.clone();
        let title_bg = title.clone();
        let content_bg = content.clone();
        tokio::spawn(async move {
            crate::integrations::gitlab::maybe_reply_task(&h_bg, &tid_bg, &node_name, &content_bg)
                .await;
            crate::integrations::im::maybe_notify(
                &h_bg,
                &uid_bg,
                "delivery",
                &format!("任务完成：{title_bg}"),
                &content_bg,
            )
            .await;
        });
    }

    Ok(Json(
        json!({"ok": true, "task_id": task_id, "status": final_status}),
    ))
}

pub async fn cancel_task(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();
    let tid = task_id.clone();
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let now = now_unix();
            let changed = tx.execute(
                "UPDATE tasks SET status='canceled', revision=revision+1, updated_at=?3
                 WHERE id=?1 AND (owner_id=?2 OR requester_id=?2) AND status IN ('pending','running')",
                params![tid, uid, now],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            tx.execute(
                "UPDATE node_jobs SET status='failed', lease_hash=NULL, lease_until=NULL, updated_at=?2
                 WHERE task_id=?1 AND status IN ('queued','running')",
                params![tid, now],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    hub.events.emit(
        user.id,
        "task",
        json!({"task_id": task_id, "status": "canceled"}),
    );
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
  echo "Usage: curl -fsSL $HUB/node/install.sh | sh -s -- <agenct_node_token>" >&2
  exit 2
fi
DIR="$HOME/.agenct"
mkdir -p "$DIR" && chmod 700 "$DIR"
curl -fsSL "$HUB/node/agenct-node.py" -o "$DIR/agenct-node.py"
chmod 700 "$DIR/agenct-node.py"
exec python3 "$DIR/agenct-node.py" --hub "$HUB" --token "$TOKEN"
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
    use super::sanitize_terminal_text;

    #[test]
    fn strips_ansi_csi_osc_and_control_bytes_preserving_utf8() {
        let raw = "\x1b[0m\n\x1b[32;1m活跃会话\x1b[0m\r\n\x1b]0;window title\x07- `agy`: 3 windows\n\t- `cc`: 5 windows\x00";
        assert_eq!(
            sanitize_terminal_text(raw),
            "\n活跃会话\n- `agy`: 3 windows\n\t- `cc`: 5 windows"
        );
    }
}
