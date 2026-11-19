//! Lightweight cross-person collaboration: @colleague delegation & manual accept/deliver.
use crate::{
    attachments, automation,
    config::{now_unix, truncate_bytes},
    db::record_audit,
    denied, invalid, nodes, Hub, Result,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
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

    let (peer_id, peer_name, task_id, mode) = hub
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

            let (task_id, mode) = if perms.auto_accept {
                if let Some(node) = nodes::pick_node_sync(conn, &peer_id, None, false)? {
                    let (tid, _) = nodes::create_task_and_enqueue_sync(
                        conn,
                        &req_id,
                        &peer_id,
                        &node.id,
                        &title,
                        &instr,
                        "delegation",
                    )?;
                    (tid, "auto_queued")
                } else {
                    let tid = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
                    conn.execute(
                        "INSERT INTO tasks(id, requester_id, owner_id, title, instruction, status, revision, created_at, updated_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6)",
                        params![tid, req_id, peer_id, title, instr, now],
                    )?;
                    (tid, "pending_no_node")
                }
            } else {
                let tid = format!("tsk_{}", &Uuid::new_v4().simple().to_string()[..12]);
                conn.execute(
                    "INSERT INTO tasks(id, requester_id, owner_id, title, instruction, status, revision, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6, ?6)",
                    params![tid, req_id, peer_id, title, instr, now],
                )?;
                (tid, "pending_manual")
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
            Ok((peer_id, peer_name, task_id, mode))
        })
        .await?;

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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskActionInput {
    pub action: String, // "assign_node" | "deliver" | "cancel"
    #[serde(default)]
    pub node_id: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

/// Unified lightweight task action endpoint (assign to node / manually deliver / cancel).
pub async fn task_action(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
    Json(input): Json<TaskActionInput>,
) -> Result<Json<Value>> {
    let user = hub.require_user(&headers).await?;
    let uid = user.id.clone();
    let tid = task_id.clone();

    match input.action.as_str() {
        "cancel" => nodes::cancel_task(State(hub), headers, Path(task_id)).await,
        "assign_node" => {
            let wanted_node = input.node_id.clone();
            let node_id = hub
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let task: Option<(String, i64)> = tx
                        .query_row(
                            "SELECT status, revision FROM tasks WHERE id = ?1 AND owner_id = ?2",
                            params![tid, uid],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let Some((status, rev)) = task else {
                        return Err(denied());
                    };
                    if !matches!(status.as_str(), "pending" | "failed") {
                        return Err(invalid("task cannot be assigned in current status"));
                    }
                    let node = nodes::pick_node_sync(&tx, &uid, wanted_node.as_deref(), false)?
                        .ok_or_else(|| invalid("no registered node available"))?;
                    let now = now_unix();
                    let next_rev = rev + 1;
                    tx.execute(
                        "UPDATE tasks SET status='running', revision=?2, updated_at=?3 WHERE id=?1",
                        params![tid, next_rev, now],
                    )?;
                    tx.execute(
                        "INSERT INTO node_jobs(task_id, user_id, node_id, task_revision, authorization, status, updated_at)
                         VALUES (?1, ?2, ?3, ?4, 'manual', 'queued', ?5)
                         ON CONFLICT(task_id) DO UPDATE SET
                           node_id=excluded.node_id, task_revision=excluded.task_revision,
                           authorization='manual', status='queued',
                           lease_hash=NULL, lease_until=NULL, submission_hash=NULL,
                           stage='Queued', updated_at=excluded.updated_at",
                        params![tid, uid, node.id, next_rev, now],
                    )?;
                    tx.commit()?;
                    Ok(node.id)
                })
                .await?;
            hub.events.emit(
                user.id,
                "task",
                json!({"task_id": task_id, "status": "running", "node_id": node_id}),
            );
            Ok(Json(
                json!({"ok": true, "task_id": task_id, "node_id": node_id}),
            ))
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
                    let row: Option<(String, String)> = tx
                        .query_row(
                            "SELECT requester_id, title FROM tasks WHERE id = ?1 AND owner_id = ?2",
                            params![tid, uid],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .optional()?;
                    let Some((req_id, title)) = row else {
                        return Err(denied());
                    };
                    let now = now_unix();
                    tx.execute(
                        "UPDATE tasks SET status='done', result=?2, revision=revision+1, updated_at=?3
                         WHERE id=?1",
                        params![tid, clean_db, now],
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
        _ => Err(invalid("action must be assign_node, deliver, or cancel")),
    }
}
