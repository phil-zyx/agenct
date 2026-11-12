//! Participant-scoped task reads and revision-checked lifecycle transitions.
use crate::{denied, invalid, policy, worker, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub(super) struct Task {
    pub(super) id: String,
    pub(super) requester: String,
    pub(super) owner: String,
    pub(super) summary: String,
    pub(super) status: String,
    pub(super) result: Option<String>,
    pub(super) revision: i64,
}
pub(super) fn row_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        requester: r.get(1)?,
        owner: r.get(2)?,
        summary: r.get(3)?,
        status: r.get(4)?,
        result: r.get(5)?,
        revision: r.get(6)?,
    })
}
/// Requester-written shared material, visible to both participants only.
pub(super) fn details(db: &rusqlite::Connection, task: &str) -> Result<Option<serde_json::Value>> {
    Ok(db
        .query_row(
            "SELECT author,content,created_at FROM task_details WHERE task_id=?1",
            [task],
            |r| {
                Ok(serde_json::json!({"author":r.get::<_,String>(0)?,
                    "content":r.get::<_,String>(1)?,"created_at":r.get::<_,String>(2)?}))
            },
        )
        .optional()?)
}
pub(super) async fn tasks(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Vec<serde_json::Value>>> {
    let id = h.identity(&headers)?;
    let db = h.db()?;
    let mut q=db.prepare("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE requester=?1 OR owner=?1 ORDER BY rowid DESC")?;
    let rows = q
        .query_map([&id], row_task)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(rows.len());
    for task in rows {
        let shared = details(&db, &task.id)?;
        let mut value = serde_json::to_value(&task).map_err(|_| invalid("invalid task"))?;
        value["details"] = shared.map_or(serde_json::Value::Null, |d| d["content"].clone());
        if task.owner == id {
            // Owner-only: whether a private draft exists for this task version.
            // Status and receipt key only; never content, and never for the requester.
            value["my_draft"] = db
                .query_row(
                    "SELECT status,request_id FROM draft_turns WHERE owner=?1 AND task_id=?2
                     AND json_extract(binding,'$.expected_revision')=?3 ORDER BY rowid DESC LIMIT 1",
                    params![id, task.id, task.revision],
                    |r| {
                        let request_id: String = r.get(1)?;
                        Ok(serde_json::json!({
                            "status": r.get::<_, String>(0)?,
                            "automatic": request_id.starts_with(crate::drafts::AUTO_PREFIX),
                            "request_id": request_id,
                        }))
                    },
                )
                .optional()?
                .or(crate::auto_drafts::projection(&db, &id, &task.id, task.revision)?)
                .unwrap_or(serde_json::Value::Null);
        }
        out.push(value);
    }
    Ok(Json(out))
}
pub(super) async fn task_detail(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let db = h.db()?;
    let t=db.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",params![task,user],row_task).map_err(|_|denied())?;
    let (events, incomplete) = crate::audit::read(&db, &task, t.revision)?;
    let audit = serde_json::json!({
        "schema_version": 1,
        "legacy_history_incomplete": incomplete,
        "legacy_notice": if incomplete { Some("Earlier transition details and overwritten delivery results cannot be reconstructed; legacy fields remain unknown.") } else { None }
    });
    // Explicit public projection: draft IDs, contents and sequence numbers never
    // cross the participant boundary, even for the task requester.
    let mut q = db.prepare("SELECT seq,author,content,created_at,task_revision FROM deliveries WHERE task_id=?1 AND status='submitted' ORDER BY seq")?;
    let deliveries = q
        .query_map([&task], |r| {
            Ok(serde_json::json!({
                "seq":r.get::<_,i64>(0)?, "author":r.get::<_,String>(1)?,
                "content":r.get::<_,String>(2)?, "created_at":r.get::<_,String>(3)?,
                "task_revision":r.get::<_,i64>(4)?
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Json(
        serde_json::json!({"task":t,"details":details(&db,&task)?,"events":events,"deliveries":deliveries,"audit":audit,
            "timeline":{"events":events,"deliveries":deliveries,"audit":audit}}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    action: String,
    expected_revision: i64,
    result: Option<String>,
    reason: Option<String>,
    source_draft_id: Option<String>,
}
pub(super) async fn action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<Action>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let t=tx.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",params![id,user],row_task).map_err(|_|denied())?;
    let policy::Effect::Transition(next) = policy::can(
        &user,
        policy::Action::Task(&a.action),
        &policy::Resource::Task {
            requester: &t.requester,
            owner: &t.owner,
            status: &t.status,
            revision: t.revision,
            expected_revision: a.expected_revision,
        },
    )
    .map_err(Error::from)?
    else {
        unreachable!("task decision")
    };
    if a.action == "deliver"
        && a.result
            .as_ref()
            .is_none_or(|v| v.trim().is_empty() || v.len() > 8000)
    {
        return Err(invalid("delivery requires a 1..8000 byte result"));
    }
    if let Some(source) = &a.source_draft_id {
        if a.action != "deliver" || source.is_empty() || source.len() > 128 {
            return Err(invalid("draft source is only accepted for delivery"));
        }
        let valid: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM deliveries WHERE id=?1 AND task_id=?2 AND author=?3 AND status='draft' AND task_revision=?4)",
            params![source, id, user, t.revision], |r| r.get(0))?;
        if !valid {
            return Err(invalid("draft source unavailable for this task version"));
        }
    }
    let reason = if a.action == "cancel" {
        Some(policy::cancellation_reason(a.reason.as_deref()).map_err(Error::from)?)
    } else {
        if a.reason.is_some() {
            return Err(invalid("reason is only accepted for cancellation"));
        }
        None
    };
    let result = if a.action == "deliver" {
        a.result
    } else if a.action == "return" {
        None
    } else {
        t.result
    };
    if a.action == "deliver" {
        // Number submitted versions independently of private drafts. The task
        // transition, immutable content and audit event commit together.
        tx.execute(
            "INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision,source_draft_id)
             SELECT ?1,?2,COALESCE(MAX(seq),0)+1,?3,?4,'submitted',?5,?6
             FROM deliveries WHERE task_id=?2 AND status='submitted'",
            params![
                uuid::Uuid::new_v4().to_string(),
                id,
                user,
                result,
                t.revision + 1,
                a.source_draft_id
            ],
        )?;
    }
    tx.execute(
        "UPDATE tasks SET status=?1,result=?2,revision=revision+1 WHERE id=?3",
        params![next, result, id],
    )?;
    if let Some(reason) = reason {
        crate::audit::record_cancellation(&tx, &id, &user, &t.status, t.revision + 1, reason)?;
    } else {
        crate::audit::record(
            &tx,
            &id,
            &user,
            &a.action,
            Some(&t.status),
            next,
            t.revision + 1,
        )?;
    }
    // Task revision changes revoke queued work and lease capabilities atomically.
    worker::revoke_task(&tx, &id)?;
    crate::auto_drafts::revoke_task(&tx, &id)?;
    let auto_prepare = h.auto_draft
        && next == "working"
        && crate::collaboration::read(&tx, &t.owner)?
            .permissions
            .auto_prepare_drafts;
    if auto_prepare {
        crate::auto_drafts::enqueue(&tx, &t.owner, &id, t.revision + 1)?;
    }
    tx.commit()?;
    drop(db);
    let mut out = serde_json::json!({"id":id,"status":next,"revision":t.revision+1});
    if auto_prepare && user == t.owner {
        out["auto_draft"] = serde_json::json!({
            "request_id": crate::drafts::auto_request_id(&id, t.revision + 1)
        });
    }
    if auto_prepare {
        h.auto_draft_notify.notify_one();
    }
    Ok(Json(out))
}
