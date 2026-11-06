//! Owner-private, at-most-once task drafting. Never changes a task or shares output.
use crate::{
    denied, invalid, profiles,
    runtime::{self, Runtime as Backend, TurnKind, TurnRequest},
    task_context, Error, Hub, Result, Runtime,
};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub(super) const STALE: &str = "draft task revision changed before reservation";
pub(super) const STATE: &str = "draft task must be working before reservation";
pub(super) const ROLE: &str = "draft role unavailable or changed before reservation";
pub(super) const BUSY: &str = "draft runtime busy before reservation";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Input {
    request_id: String,
    expected_revision: i64,
    agent_id: Option<String>,
    expected_profile_revision: Option<i64>,
}
fn key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn owner_task(db: &Connection, owner: &str, task: &str) -> Result<(String, i64)> {
    db.query_row(
        "SELECT status,revision FROM tasks WHERE id=?1 AND owner=?2",
        params![task, owner],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .ok_or_else(denied)
}
fn receipt(db: &Connection, owner: &str, task: &str, request_id: &str) -> Result<Option<Value>> {
    let row = db.query_row("SELECT status,response FROM draft_turns WHERE owner=?1 AND task_id=?2 AND request_id=?3", params![owner,task,request_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).optional()?;
    row.map(|(status, response)| {
        let status = &status;
        let response = response.map(|s| serde_json::from_str::<Value>(&s).map_err(|_| invalid("invalid stored draft receipt"))).transpose()?;
        let mut value=json!({"kind":"draft_delivery","task_id":task,"request_id":request_id,"status":status,"response":response});
        if status == "uncertain" { value["reason"]=json!("outcome_unknown"); }
        if status == "complete" {
            let (state,revision)=owner_task(db,owner,task)?;
            value["current_stale"]=json!(state != "working" || value["response"]["draft"]["task_revision"] != revision);
        }
        Ok(value)
    }).transpose()
}
fn replay(db: &Connection, owner: &str, task: &str, input: &Input) -> Result<Option<Value>> {
    let binding = db
        .query_row(
            "SELECT task_id,binding FROM draft_turns WHERE owner=?1 AND request_id=?2",
            params![owner, input.request_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((bound_task, binding)) = binding {
        if bound_task != task
            || binding
                != serde_json::to_string(input).map_err(|_| invalid("invalid draft input"))?
        {
            return Err(Error(
                StatusCode::CONFLICT,
                "draft request_id already bound to different input",
            ));
        }
        return receipt(db, owner, task, &input.request_id);
    }
    Ok(None)
}
fn uncertain(h: &Hub, owner: &str, request_id: &str) {
    // A failed best-effort write never permits dispatch of the reserved key again.
    if let Ok(db) = h.db() {
        let _ = db.execute("UPDATE draft_turns SET status='uncertain' WHERE owner=?1 AND request_id=?2 AND status='pending'", params![owner,request_id]);
    }
}

// Cancellation drops this guard before the owner runtime gate. Best-effort
// cleanup is durable; if storage fails, pending stays blocked until recovery.
struct Reservation<'a> {
    h: &'a Hub,
    owner: &'a str,
    request_id: &'a str,
}
impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        uncertain(self.h, self.owner, self.request_id);
    }
}

pub(super) async fn create(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task): Path<String>,
    Json(input): Json<Input>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !key(&task)
        || !key(&input.request_id)
        || input.expected_revision < 0
        || input.agent_id.is_some() != input.expected_profile_revision.is_some()
        || input
            .agent_id
            .as_ref()
            .is_some_and(|id| !key(id) || id.len() > 32)
        || input.expected_profile_revision.is_some_and(|r| r < 0)
    {
        return Err(invalid("invalid draft request"));
    }
    {
        let db = h.db()?;
        owner_task(&db, &owner, &task)?;
        if let Some(value) = replay(&db, &owner, &task, &input)? {
            return Ok(Json(value));
        }
    }
    let _gate = h.gates[&owner]
        .try_lock()
        .map_err(|_| Error(StatusCode::CONFLICT, BUSY))?;
    let (profile, snapshot, session) = {
        let mut db = h.db()?;
        let tx = db.transaction()?;
        owner_task(&tx, &owner, &task)?;
        if let Some(value) = replay(&tx, &owner, &task, &input)? {
            return Ok(Json(value));
        }
        let (status, revision) = owner_task(&tx, &owner, &task)?;
        if revision != input.expected_revision {
            return Err(Error(StatusCode::CONFLICT, STALE));
        }
        if status != "working" {
            return Err(Error(StatusCode::CONFLICT, STATE));
        }
        let profile = profiles::route(
            &tx,
            &owner,
            input.agent_id.as_deref(),
            input.expected_profile_revision,
        )
        .map_err(|error| {
            if [
                StatusCode::BAD_REQUEST,
                StatusCode::CONFLICT,
                StatusCode::NOT_FOUND,
            ]
            .contains(&error.0)
            {
                Error(StatusCode::CONFLICT, ROLE)
            } else {
                error
            }
        })?;
        let selection = task_context::Selection {
            task_id: task.clone(),
            expected_revision: revision,
        };
        let snapshot = task_context::capture(&tx, &owner, Some(&selection))?.ok_or_else(denied)?;
        let session = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO draft_turns(owner,request_id,task_id,binding,status,task_context,runtime_profile,session_id) VALUES (?1,?2,?3,?4,'pending',?5,?6,?7)",
            params![owner,input.request_id,task,serde_json::to_string(&input).map_err(|_|invalid("invalid draft input"))?,snapshot.to_string(),serde_json::to_string(&profile).map_err(|_|invalid("invalid runtime profile"))?,session])?;
        tx.commit()?;
        (profile, snapshot, session)
    };
    let _reservation = Reservation {
        h: &h,
        owner: &owner,
        request_id: &input.request_id,
    };
    let request = TurnRequest {
        kind: TurnKind::DraftDelivery,
        message: runtime::DRAFT_MESSAGE,
        session_id: &session,
        history: &[],
        profile: &profile,
        task_context: Some(&snapshot),
    };
    let answer = match &h.runtime {
        Runtime::Mock => runtime::MockRuntime.turn(request).await,
        Runtime::Dsh { url, token } => {
            runtime::DshRuntime {
                client: &h.client,
                url,
                token,
            }
            .turn(request)
            .await
        }
    };
    let completed = (|| -> Result<Value> {
        let answer = answer?;
        if !answer.valid_draft(h.runtime.label()) {
            return Err(runtime::adapter_failed());
        }
        let mut db = h.db()?;
        let tx = db.transaction()?;
        let state = owner_task(&tx, &owner, &task)?;
        let stale = state.0 != "working" || state.1 != input.expected_revision;
        let draft_id = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision) SELECT ?1,?2,COALESCE(MAX(seq),0)+1,?3,?4,'draft',?5 FROM deliveries WHERE task_id=?2 AND status='draft'",
            params![draft_id,task,owner,answer.reply,input.expected_revision])?;
        let created_at: String = tx.query_row(
            "SELECT created_at FROM deliveries WHERE id=?1",
            [&draft_id],
            |r| r.get(0),
        )?;
        let response = json!({"draft":{"id":draft_id,"content":answer.reply,"created_at":created_at,"task_revision":input.expected_revision,"stale":stale},"task_context":snapshot,"runtime_profile":profile,"backend":h.runtime.label()});
        if tx.execute("UPDATE draft_turns SET status='complete',response=?3,draft_id=?4 WHERE owner=?1 AND request_id=?2 AND status='pending'",params![owner,input.request_id,response.to_string(),draft_id])? != 1 {
            return Err(Error(StatusCode::CONFLICT,"draft reservation unavailable"));
        }
        tx.commit()?;
        receipt(&db, &owner, &task, &input.request_id)?.ok_or_else(denied)
    })();
    if completed.is_err() {
        uncertain(&h, &owner, &input.request_id);
    }
    completed.map(Json)
}

pub(super) async fn status(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((task, request_id)): Path<(String, String)>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !key(&task) || !key(&request_id) {
        return Err(denied());
    }
    let db = h.db()?;
    owner_task(&db, &owner, &task)?;
    Ok(Json(
        receipt(&db, &owner, &task, &request_id)?.ok_or_else(denied)?,
    ))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Page {
    after: Option<String>,
    limit: Option<usize>,
}
pub(super) async fn list(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task): Path<String>,
    Query(page): Query<Page>,
) -> Result<Json<Value>> {
    let owner = h.identity(&headers)?;
    if !key(&task)
        || page.after.as_ref().is_some_and(|s| !key(s))
        || page.limit.is_some_and(|n| !(1..=50).contains(&n))
    {
        return Err(invalid("invalid draft page"));
    }
    let db = h.db()?;
    owner_task(&db, &owner, &task)?;
    let limit = page.limit.unwrap_or(20);
    let mut query = db.prepare("SELECT request_id FROM draft_turns WHERE owner=?1 AND task_id=?2 AND request_id>?3 ORDER BY request_id LIMIT ?4")?;
    let keys = query
        .query_map(
            params![owner, task, page.after.unwrap_or_default(), limit + 1],
            |r| r.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let next = if keys.len() > limit {
        keys.get(limit - 1).cloned()
    } else {
        None
    };
    let mut items = Vec::new();
    for request_id in keys.iter().take(limit) {
        items.push(receipt(&db, &owner, &task, request_id)?.ok_or_else(denied)?);
    }

    Ok(Json(
        json!({"kind":"draft_delivery","task_id":task,"items":items,"next_cursor":next}),
    ))
}

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS draft_turns (
 owner TEXT NOT NULL, request_id TEXT NOT NULL, task_id TEXT NOT NULL REFERENCES tasks(id),
 binding TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('pending','complete','uncertain')),
 task_context TEXT NOT NULL, runtime_profile TEXT NOT NULL, session_id TEXT NOT NULL,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, response TEXT, draft_id TEXT REFERENCES deliveries(id),
 PRIMARY KEY(owner,request_id), UNIQUE(draft_id),
 CHECK ((status='complete' AND response IS NOT NULL AND draft_id IS NOT NULL) OR
        (status!='complete' AND response IS NULL AND draft_id IS NULL)));
CREATE TRIGGER IF NOT EXISTS draft_turns_no_replace BEFORE INSERT ON draft_turns
WHEN EXISTS(SELECT 1 FROM draft_turns WHERE owner=NEW.owner AND request_id=NEW.request_id)
 OR (NEW.draft_id IS NOT NULL AND EXISTS(SELECT 1 FROM draft_turns WHERE draft_id=NEW.draft_id))
BEGIN SELECT RAISE(ABORT,'draft receipt already exists'); END;
CREATE TRIGGER IF NOT EXISTS draft_turns_no_replace_update BEFORE UPDATE OF draft_id ON draft_turns
WHEN NEW.draft_id IS NOT NULL AND EXISTS(SELECT 1 FROM draft_turns WHERE draft_id=NEW.draft_id AND (owner!=OLD.owner OR request_id!=OLD.request_id))
BEGIN SELECT RAISE(ABORT,'draft receipt already exists'); END;
CREATE TRIGGER IF NOT EXISTS draft_turns_no_delete BEFORE DELETE ON draft_turns
BEGIN SELECT RAISE(ABORT,'draft receipts are durable'); END;
CREATE TRIGGER IF NOT EXISTS draft_turns_binding_immutable BEFORE UPDATE OF owner,request_id,task_id,binding,task_context,runtime_profile,session_id,created_at ON draft_turns
BEGIN SELECT RAISE(ABORT,'draft receipt binding is immutable'); END;
CREATE TRIGGER IF NOT EXISTS draft_turns_terminal BEFORE UPDATE OF status,response,draft_id ON draft_turns
WHEN OLD.status!='pending' OR NEW.status NOT IN ('complete','uncertain')
BEGIN SELECT RAISE(ABORT,'draft receipt outcome is terminal'); END;";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app, Config, UserConfig};
    use axum::{body::Body, http::Request, routing::post, Router};
    use http_body_util::BodyExt;
    use sha2::{Digest, Sha256};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tower::ServiceExt;
    #[tokio::test]
    async fn real_timeout_is_uncertain_and_never_retries_or_writes_delivery() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let adapter = Router::new().route(
            "/turn",
            post(move || {
                let count = counter.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<Json<Value>>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let token = "x".repeat(40);
        let mut h = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: vec![UserConfig {
                id: "bob".into(),
                token_sha256: format!("{:x}", Sha256::digest(token.as_bytes())),
            }],
            runtime: Runtime::Dsh {
                url,
                token: "internal".into(),
            },
        })
        .unwrap();
        h.client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_millis(50))
            .build()
            .unwrap();
        h.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','shared','working',1)",[]).unwrap();
        let a = app(h.clone());
        for index in 0..2 {
            let r = a
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/tasks/task/drafts")
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(
                            r#"{"request_id":"timeout","expected_revision":1}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            if index == 0 {
                assert_eq!(r.status(), StatusCode::BAD_GATEWAY);
            } else {
                assert_eq!(r.status(), StatusCode::OK);
                let bytes = r.into_body().collect().await.unwrap().to_bytes();
                assert_eq!(
                    serde_json::from_slice::<Value>(&bytes).unwrap()["status"],
                    "uncertain"
                );
            }
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT count(*) FROM deliveries", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        server.abort();
    }
}
