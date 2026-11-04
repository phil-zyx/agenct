//! Queue admission only. No executor, provider, publication or worker-claim routes.
use crate::{denied, invalid, worker, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};

/// Operator-owned exact task grant. This grants queue admission, NOT execution.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub task_id: String,
    pub owner: String,
    pub task_revision: i64,
    pub repository: String,
    pub base_commit: String,
}
impl Hub {
    /// Empty by default. Scopes can only be installed by the server operator.
    /// No executable path, command, credentials or publication permission is accepted.
    pub fn with_worker_scopes(
        mut self,
        scopes: Vec<Scope>,
    ) -> std::result::Result<Self, &'static str> {
        if scopes.len() > 256 {
            return Err("at most 256 worker scopes are allowed");
        }
        let mut entries = HashMap::new();
        for scope in scopes {
            if !self.gates.contains_key(&scope.owner)
                || scope.task_id.is_empty()
                || scope.task_id.len() > 128
                || scope.task_revision < 1
                || scope.repository.is_empty()
                || scope.repository.len() > 128
                || !scope
                    .repository
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
                || !matches!(scope.base_commit.len(), 40 | 64)
                || !scope
                    .base_commit
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || entries.contains_key(&scope.task_id)
            {
                return Err("invalid or duplicate worker scope");
            }
            entries.insert(scope.task_id.clone(), scope);
        }
        self.worker_scopes = std::sync::Arc::new(entries);
        Ok(self)
    }
}
fn map_error(error: worker::Error) -> Error {
    match error {
        worker::Error::Denied => denied(),
        worker::Error::Invalid(_) => invalid("invalid worker request"),
        worker::Error::Conflict | worker::Error::LeaseLost => Error(
            StatusCode::CONFLICT,
            "worker request conflicts; refresh and review",
        ),
        worker::Error::ExecutorUnavailable => {
            Error(StatusCode::SERVICE_UNAVAILABLE, "executor unavailable")
        }
        worker::Error::Database(_) => Error(StatusCode::INTERNAL_SERVER_ERROR, "storage error"),
    }
}
fn owned_task(db: &rusqlite::Connection, actor: &str, id: &str) -> Result<(String, i64)> {
    db.query_row(
        "SELECT status,revision FROM tasks WHERE id=?1 AND owner=?2",
        params![id, actor],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .map_err(|_| denied())
}
/// Shows only the authenticated task owner's queue and exact current admission scope.
pub(crate) async fn status(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let (status, revision) = owned_task(&db, &actor, &id)?;
    let scope = h
        .worker_scopes
        .get(&id)
        .filter(|s| s.owner == actor && s.task_revision == revision && status == "working");
    let jobs = worker::list_for_task(&db, &actor, &id).map_err(map_error)?;
    Ok(Json(
        serde_json::json!({"scope":scope,"jobs":jobs,"execution_enabled":false,"publication_enabled":false}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Enqueue {
    expected_revision: i64,
    repository: String,
    base_commit: String,
    request_id: String,
    /// A separate owner action, not implied by accepting a delegated task.
    confirm_queue_only: bool,
}
pub(crate) async fn enqueue(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Enqueue>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    owned_task(&db, &actor, &id)?;
    if !input.confirm_queue_only {
        return Err(invalid("explicit queue-only confirmation required"));
    }
    let scope = h
        .worker_scopes
        .get(&id)
        .filter(|s| s.owner == actor)
        .ok_or_else(denied)?;
    if scope.task_revision != input.expected_revision
        || scope.repository != input.repository
        || scope.base_commit != input.base_commit
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "request does not match operator-approved task scope",
        ));
    }
    // This reserved ID is only an admission marker. No consumer is constructed or
    // scheduled, and the built-in executor's hard-disable remains unconditional.
    let policy = worker::Policy {
        executor_id: Some("queue-only-disabled".into()),
        repositories: BTreeSet::from([scope.repository.clone()]),
        required_checks: BTreeSet::new(),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("clock unavailable"))?
        .as_secs();
    let now = i64::try_from(now).map_err(|_| invalid("clock unavailable"))?;
    let job = worker::enqueue_queue_only(
        &mut db,
        &actor,
        worker::JobRequest {
            task_id: id,
            task_revision: input.expected_revision,
            repository: input.repository,
            base_commit: input.base_commit,
            request_id: input.request_id,
        },
        &policy,
        now,
    )
    .map_err(map_error)?;
    Ok(Json(
        serde_json::json!({"job":job,"execution_enabled":false,"publication_enabled":false}),
    ))
}
pub(crate) async fn cancel(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((task, job)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    owned_task(&db, &actor, &task)?;
    let (record, _) = worker::inspect(&db, &actor, &job).map_err(map_error)?;
    if record.request.task_id != task {
        return Err(denied());
    }
    worker::cancel(&db, &actor, &job).map_err(map_error)?;
    Ok(Json(
        serde_json::json!({"id":job,"status":"cancelled","execution_enabled":false,"publication_enabled":false}),
    ))
}
