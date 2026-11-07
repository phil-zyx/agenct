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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    revision: Option<Revision>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    base_draft_id: String,
    instruction: String,
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
    generate(&h, &owner, &task, input).await.map(Json)
}

/// Prefix of the Hub-chosen request ID for an automatic draft. Deterministic per
/// task revision, so a task version is drafted automatically at most once.
pub(super) const AUTO_PREFIX: &str = "auto-";
pub(super) fn auto_request_id(task: &str, revision: i64) -> String {
    format!("{AUTO_PREFIX}{task}-r{revision}")
}

/// Starts the owner's private draft in the background once a task is (back) in
/// `working`. It is exactly the draft a click would make: owner-only, the owner's
/// recommended enabled role, same receipt and at-most-once rules; nothing is
/// shared, delivered or approved without the owner. Skipped silently if the
/// task moves on first; waits briefly while the owner's own turn is running.
pub(super) fn spawn_auto(h: Hub, owner: String, task: String, revision: i64) {
    tokio::spawn(async move {
        for _ in 0..40 {
            let profile = {
                let Ok(db) = h.db() else { return };
                let summary: Option<String> = db
                    .query_row(
                        "SELECT summary FROM tasks WHERE id=?1 AND owner=?2 AND status='working' AND revision=?3",
                        params![task, owner, revision],
                        |r| r.get(0),
                    )
                    .optional()
                    .ok()
                    .flatten();
                let Some(summary) = summary else { return };
                match profiles::recommend(&db, &owner, &summary) {
                    Ok(profile) => profile,
                    Err(_) => return,
                }
            };
            let input = Input {
                request_id: auto_request_id(&task, revision),
                expected_revision: revision,
                agent_id: Some(profile.agent_id.clone()),
                expected_profile_revision: Some(profile.revision as i64),
                revision: None,
            };
            match generate_inner(&h, &owner, &task, input, true).await {
                Err(Error(StatusCode::CONFLICT, BUSY)) => {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await
                }
                _ => return,
            }
        }
    });
}

pub(super) async fn generate(h: &Hub, owner: &str, task: &str, input: Input) -> Result<Value> {
    generate_inner(h, owner, task, input, false).await
}

// Refresh live membership before each new automatic model call. This cannot
// retract a request already sent, and credential rotation alone is not removal.
fn auto_authorized(h: &Hub, owner: &str, task: &str, revision: i64) -> Result<()> {
    h.reload_private_users()?;
    if !h.directory.contains(owner) {
        return Err(denied());
    }
    let state = owner_task(&*h.db()?, owner, task)?;
    if state.0 != "working" || state.1 != revision {
        return Err(Error(StatusCode::CONFLICT, STALE));
    }
    Ok(())
}

async fn generate_inner(
    h: &Hub,
    owner: &str,
    task: &str,
    input: Input,
    automatic: bool,
) -> Result<Value> {
    let (owner, task) = (owner.to_owned(), task.to_owned());
    let authorized = || {
        if automatic {
            auto_authorized(h, &owner, &task, input.expected_revision)
        } else {
            Ok(())
        }
    };
    authorized()?;
    if !key(&task)
        || !key(&input.request_id)
        || input.expected_revision < 0
        || input.agent_id.is_some() != input.expected_profile_revision.is_some()
        || input
            .agent_id
            .as_ref()
            .is_some_and(|id| !key(id) || id.len() > 32)
        || input.expected_profile_revision.is_some_and(|r| r < 0)
        || input.revision.as_ref().is_some_and(|r| {
            !key(&r.base_draft_id) || r.instruction.trim().is_empty() || r.instruction.len() > 4000
        })
    {
        return Err(invalid("invalid draft request"));
    }
    {
        let db = h.db()?;
        owner_task(&db, &owner, &task)?;
        if let Some(value) = replay(&db, &owner, &task, &input)? {
            return Ok(value);
        }
    }
    let _gate = h
        .gate(&owner)?
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, BUSY))?;
    authorized()?;
    let (profile, snapshot, session, revision_context) = {
        let mut db = h.db()?;
        let tx = db.transaction()?;
        owner_task(&tx, &owner, &task)?;
        if let Some(value) = replay(&tx, &owner, &task, &input)? {
            return Ok(value);
        }
        // The same owner gate serializes manual and automatic reservations.
        // Any other attempt for this version (including uncertain) suppresses
        // background work; only a new explicit manual request may try again.
        if automatic {
            let existing: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM draft_turns WHERE owner=?1 AND task_id=?2 AND request_id!=?3 AND json_extract(binding,'$.expected_revision')=?4)",
                params![owner, task, input.request_id, input.expected_revision],
                |r| r.get(0),
            )?;
            if existing {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "automatic draft superseded by an existing attempt",
                ));
            }
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
        // Resolve the immutable base server-side. Neither another owner's draft nor a
        // draft from a previous task revision can enter this private model request.
        let revision_context = input.revision.as_ref().map(|change| -> Result<Value> {
            let content: Option<String> = tx.query_row(
                "SELECT content FROM deliveries WHERE id=?1 AND task_id=?2 AND author=?3 AND status='draft' AND task_revision=?4",
                params![change.base_draft_id, task, owner, revision], |r| r.get(0)).optional()?;
            let content = content.ok_or_else(|| invalid("base draft unavailable for this task revision"))?;
            Ok(json!({"base_draft_id":change.base_draft_id,"content":content,"instruction":change.instruction}))
        }).transpose()?;
        let session = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO draft_turns(owner,request_id,task_id,binding,status,task_context,runtime_profile,session_id) VALUES (?1,?2,?3,?4,'pending',?5,?6,?7)",
            params![owner,input.request_id,task,serde_json::to_string(&input).map_err(|_|invalid("invalid draft input"))?,snapshot.to_string(),serde_json::to_string(&profile).map_err(|_|invalid("invalid runtime profile"))?,session])?;
        tx.commit()?;
        (profile, snapshot, session, revision_context)
    };
    let _reservation = Reservation {
        h,
        owner: &owner,
        request_id: &input.request_id,
    };
    // Reserve before any summarization/model call: even a lost summary response
    // leaves this request uncertain rather than permitting duplicate billing.
    let source_snapshot = snapshot.clone();
    let snapshot =
        task_context::prepare_large_details_with_guard(h, &owner, &profile, snapshot, &authorized)
            .await?;
    let request = TurnRequest {
        kind: TurnKind::DraftDelivery,
        message: runtime::DRAFT_MESSAGE,
        session_id: &session,
        history: &[],
        profile: &profile,
        task_context: Some(&snapshot),
        draft_revision: revision_context.as_ref(),
        task_role: None,
    };
    authorized()?;
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
        let seq: i64 =
            tx.query_row("SELECT seq FROM deliveries WHERE id=?1", [&draft_id], |r| {
                r.get(0)
            })?;
        let created_at: String = tx.query_row(
            "SELECT created_at FROM deliveries WHERE id=?1",
            [&draft_id],
            |r| r.get(0),
        )?;
        let response = json!({"draft":{"id":draft_id,"seq":seq,"base_draft_id":input.revision.as_ref().map(|r| &r.base_draft_id),"instruction":input.revision.as_ref().map(|r| &r.instruction),"content":answer.reply,"created_at":created_at,"task_revision":input.expected_revision,"stale":stale},"task_context":snapshot,"source_task_context":source_snapshot,"runtime_profile":profile,"backend":h.runtime.label()});
        if tx.execute("UPDATE draft_turns SET status='complete',response=?3,draft_id=?4 WHERE owner=?1 AND request_id=?2 AND status='pending'",params![owner,input.request_id,response.to_string(),draft_id])? != 1 {
            return Err(Error(StatusCode::CONFLICT,"draft reservation unavailable"));
        }
        tx.commit()?;
        receipt(&db, &owner, &task, &input.request_id)?.ok_or_else(denied)
    })();
    if completed.is_err() {
        uncertain(h, &owner, &input.request_id);
    }
    completed
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
    let summary: String = db.query_row("SELECT summary FROM tasks WHERE id=?1", [&task], |r| {
        r.get(0)
    })?;
    let recommended = profiles::recommend(&db, &owner, &summary)?;
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
        json!({"kind":"draft_delivery","task_id":task,"items":items,"next_cursor":next,"recommended_profile":recommended}),
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

    struct AutoFixture {
        h: Hub,
        root: std::path::PathBuf,
        roster: std::path::PathBuf,
        calls: Arc<AtomicUsize>,
        server: tokio::task::JoinHandle<()>,
    }
    impl Drop for AutoFixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    fn roster_value(include_bob: bool) -> String {
        let users: Vec<_> = ["alice", "bob"]
            .into_iter()
            .filter(|id| *id != "bob" || include_bob)
            .map(
                |id| json!({"id":id,"token_sha256":format!("{:x}", Sha256::digest(id.as_bytes()))}),
            )
            .collect();
        json!({"version":1,"port":8080,"runtime":"dsh","users":users}).to_string()
    }
    impl AutoFixture {
        async fn new(revoke_after_first_call: bool, fail: bool) -> Self {
            let root = std::env::temp_dir().join(format!("agenct-auto-safety-{}", Uuid::new_v4()));
            std::fs::create_dir(&root).unwrap();
            let roster = root.join("config.json");
            std::fs::write(&roster, roster_value(true)).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&roster, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let counter = calls.clone();
            let config_file = roster.clone();
            let adapter = Router::new().route(
                "/turn",
                post(move |Json(request): Json<Value>| {
                    let count = counter.clone();
                    let config_file = config_file.clone();
                    async move {
                        // Integration with chat actions must not grant the private
                        // draft or summary model permission to suggest task actions.
                        assert!(request.get("task_role").is_none());
                        assert!(matches!(
                            request["kind"].as_str(),
                            Some("draft_delivery" | "context_summary")
                        ));
                        if request["kind"] == "draft_delivery" {
                            assert_eq!(request["history"], json!([]));
                        }
                        let previous = count.fetch_add(1, Ordering::SeqCst);
                        if revoke_after_first_call && previous == 0 {
                            std::fs::write(config_file, roster_value(false)).unwrap();
                        }
                        (
                            if fail { StatusCode::BAD_GATEWAY } else { StatusCode::OK },
                            Json(json!({"backend":"dsh","reply":"FAKE adapter response, no live model.","proposal":null})),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/turn", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
            let h = Hub::open(Config {
                bind: "127.0.0.1:0".parse().unwrap(),
                database: ":memory:".into(),
                users: ["alice", "bob"]
                    .map(|id| UserConfig {
                        id: id.into(),
                        token_sha256: format!("{:x}", Sha256::digest(id.as_bytes())),
                    })
                    .into(),
                runtime: Runtime::Dsh {
                    url,
                    token: "fake-adapter-token".into(),
                },
            })
            .unwrap()
            .with_private_config(roster.clone())
            .unwrap();
            h.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','shared','working',1)", []).unwrap();
            Self {
                h,
                root,
                roster,
                calls,
                server,
            }
        }
        fn input(&self, automatic: bool) -> Input {
            Input {
                request_id: if automatic {
                    auto_request_id("task", 1)
                } else {
                    "manual".into()
                },
                expected_revision: 1,
                agent_id: None,
                expected_profile_revision: None,
                revision: None,
            }
        }
        fn receipt_count(&self) -> i64 {
            self.h
                .db()
                .unwrap()
                .query_row("SELECT count(*) FROM draft_turns", [], |r| r.get(0))
                .unwrap()
        }
    }

    #[tokio::test]
    async fn automatic_retry_checks_removed_owner_and_broken_roster_before_dispatch() {
        for malformed in [false, true] {
            let f = AutoFixture::new(false, false).await;
            let gate = f.h.gate("bob").unwrap().lock_owned().await;
            let first = generate_inner(&f.h, "bob", "task", f.input(true), true).await;
            assert!(matches!(first, Err(Error(StatusCode::CONFLICT, BUSY))));
            assert_eq!(f.receipt_count(), 0);
            std::fs::write(
                &f.roster,
                if malformed {
                    "broken".into()
                } else {
                    roster_value(false)
                },
            )
            .unwrap();
            drop(gate);
            // Same entry point as the next scheduled automatic attempt. It must
            // refresh even when no intervening authenticated request did so.
            assert!(generate_inner(&f.h, "bob", "task", f.input(true), true)
                .await
                .is_err());
            assert_eq!(f.calls.load(Ordering::SeqCst), 0);
            assert_eq!(f.receipt_count(), 0);
        }
    }

    #[tokio::test]
    async fn automatic_retry_does_not_duplicate_completed_or_uncertain_manual_attempt() {
        for fail in [false, true] {
            let f = AutoFixture::new(false, fail).await;
            let gate = f.h.gate("bob").unwrap().lock_owned().await;
            assert!(matches!(
                generate_inner(&f.h, "bob", "task", f.input(true), true).await,
                Err(Error(StatusCode::CONFLICT, BUSY))
            ));
            drop(gate);
            let manual = generate(&f.h, "bob", "task", f.input(false)).await;
            assert_eq!(manual.is_err(), fail);
            assert_eq!(f.calls.load(Ordering::SeqCst), 1);
            assert!(generate_inner(&f.h, "bob", "task", f.input(true), true)
                .await
                .is_err());
            assert_eq!(f.calls.load(Ordering::SeqCst), 1);
            assert_eq!(f.receipt_count(), 1);
            let mut explicit_retry = f.input(false);
            explicit_retry.request_id = "manual-explicit-retry".into();
            let _ = generate(&f.h, "bob", "task", explicit_retry).await;
            assert_eq!(f.calls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn removal_during_summary_blocks_next_summary_or_final_draft_without_redispatch() {
        // One summary chunk tests the final draft boundary; multiple chunks test
        // revocation between summary calls. All responses come from the fake adapter.
        for size in [16000, 90000] {
            let f = AutoFixture::new(true, false).await;
            f.h.db()
                .unwrap()
                .execute(
                    "INSERT INTO task_details(task_id,author,content) VALUES ('task','alice',?1)",
                    ["x".repeat(size)],
                )
                .unwrap();
            assert!(generate_inner(&f.h, "bob", "task", f.input(true), true)
                .await
                .is_err());
            assert_eq!(f.calls.load(Ordering::SeqCst), 1);
            {
                let db = f.h.db().unwrap();
                let state: String = db
                    .query_row("SELECT status FROM draft_turns", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(state, "uncertain");
                let count: i64 = db
                    .query_row("SELECT count(*) FROM deliveries", [], |r| r.get(0))
                    .unwrap();
                assert_eq!(count, 0);
            }
            std::fs::write(&f.roster, roster_value(true)).unwrap();
            let replay = generate_inner(&f.h, "bob", "task", f.input(true), true)
                .await
                .unwrap();
            assert_eq!(replay["status"], "uncertain");
            assert_eq!(f.calls.load(Ordering::SeqCst), 1);
        }
    }
}
