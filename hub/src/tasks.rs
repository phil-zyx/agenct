//! Participant-scoped task reads and revision-checked lifecycle transitions.
use crate::{denied, invalid, worker, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
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
pub(super) async fn tasks(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Task>>> {
    let id = h.identity(&headers)?;
    let db = h.db()?;
    let mut q=db.prepare("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE requester=?1 OR owner=?1 ORDER BY rowid DESC")?;
    let rows = q.query_map([id], row_task)?;
    Ok(Json(rows.collect::<std::result::Result<Vec<_>, _>>()?))
}
pub(super) async fn task_detail(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let db = h.db()?;
    let t=db.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",params![task,user],row_task).map_err(|_|denied())?;
    let mut q = db.prepare("SELECT actor,action,at FROM events WHERE task_id=?1 ORDER BY id")?;
    let events=q.query_map([task],|r|Ok(serde_json::json!({"actor":r.get::<_,String>(0)?,"action":r.get::<_,String>(1)?,"at":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(Json(serde_json::json!({"task":t,"events":events})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    action: String,
    expected_revision: i64,
    result: Option<String>,
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
    if a.expected_revision != t.revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "task changed; refresh and review before acting",
        ));
    }
    let next = match (a.action.as_str(), t.status.as_str()) {
        ("accept", "awaiting_owner") if user == t.owner => "working",
        ("deliver", "working") if user == t.owner => {
            if a.result
                .as_ref()
                .is_none_or(|v| v.trim().is_empty() || v.len() > 8000)
            {
                return Err(invalid("delivery requires a 1..8000 byte result"));
            }
            "ready_for_acceptance"
        }
        ("approve", "ready_for_acceptance") if user == t.requester => "completed",
        ("return", "ready_for_acceptance") if user == t.requester => "awaiting_owner",
        ("cancel", s) if s != "completed" && s != "canceled" => "canceled",
        _ => {
            return Err(Error(
                StatusCode::CONFLICT,
                "action not allowed for identity or current state",
            ))
        }
    };
    let result = if a.action == "deliver" {
        a.result
    } else if a.action == "return" {
        None
    } else {
        t.result
    };
    tx.execute(
        "UPDATE tasks SET status=?1,result=?2,revision=revision+1 WHERE id=?3",
        params![next, result, id],
    )?;
    tx.execute(
        "INSERT INTO events(task_id,actor,action) VALUES (?1,?2,?3)",
        params![id, user, a.action],
    )?;
    // Task revision changes revoke queued work and lease capabilities atomically.
    worker::revoke_task(&tx, &id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"id":id,"status":next,"revision":t.revision+1}),
    ))
}
