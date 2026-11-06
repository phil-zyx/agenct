//! Queue admission only. No executor, provider, publication or worker-claim routes.
use crate::{denied, invalid, policy, worker, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
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
            if !self.directory.contains(&scope.owner)
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
    let (owner, status, revision): (String, String, i64) = db
        .query_row(
            "SELECT owner,status,revision FROM tasks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .map_err(|_| denied())?;
    policy::can(
        actor,
        policy::Action::WorkerAccess,
        &policy::Resource::WorkerTask {
            owner: &owner,
            status: &status,
            revision,
            expected_revision: revision,
        },
    )
    .map_err(Error::from)?;
    Ok((status, revision))
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
    let scope = h.worker_scopes.get(&id).filter(|s| {
        policy::can(
            &actor,
            policy::Action::WorkerExecute,
            &policy::Resource::WorkerTask {
                owner: &s.owner,
                status: &status,
                revision,
                expected_revision: s.task_revision,
            },
        )
        .is_ok()
    });
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
    let scope = h.worker_scopes.get(&id);
    policy::can(
        &actor,
        policy::Action::WorkerEnqueue,
        &policy::Resource::WorkerQueue {
            owner: &actor,
            confirmed: input.confirm_queue_only,
            scope: scope.map(|s| policy::QueueScope {
                owner: &s.owner,
                revision: s.task_revision,
                repository: &s.repository,
                base_commit: &s.base_commit,
            }),
            expected_revision: input.expected_revision,
            repository: &input.repository,
            base_commit: &input.base_commit,
        },
    )
    .map_err(Error::from)?;
    let scope = scope.expect("policy requires an approved scope");
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
    policy::can(
        &actor,
        policy::Action::WorkerCancel,
        &policy::Resource::WorkerJob {
            owner: &record.owner,
            status: &record.status,
            task_id: &record.request.task_id,
            expected_task_id: &task,
        },
    )
    .map_err(Error::from)?;
    worker::cancel(&db, &actor, &job).map_err(map_error)?;
    Ok(Json(
        serde_json::json!({"id":job,"status":"cancelled","execution_enabled":false,"publication_enabled":false}),
    ))
}
