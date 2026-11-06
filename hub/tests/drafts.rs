use agenct_hub::{app, Config, Hub, Runtime, UserConfig};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
fn token(id: &str) -> String {
    format!("{id}-{}", "x".repeat(40))
}
fn config(path: &str) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: path.into(),
        users: ["alice", "bob", "eve"]
            .map(|id| UserConfig {
                id: id.into(),
                token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
            })
            .into(),
        runtime: Runtime::Mock,
    }
}
async fn request(
    app: &Router,
    who: &str,
    method: &str,
    path: &str,
    data: Value,
) -> (StatusCode, Value) {
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {}", token(who)))
                .header("content-type", "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(data.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = r.status();
    let body = r.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::Semaphore;

struct Adapter {
    url: String,
    calls: Arc<AtomicUsize>,
    payloads: Arc<std::sync::Mutex<Vec<Value>>>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Adapter {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Adapter {
    async fn new(reply: Value, blocked: bool) -> Self {
        let payloads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = payloads.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(if blocked { 0 } else { 100 }));
        let (c, e, r) = (calls.clone(), entered.clone(), release.clone());
        let adapter = Router::new().route(
            "/v1/turn",
            axum::routing::post(move |axum::Json(payload): axum::Json<Value>| {
                capture.lock().unwrap().push(payload);
                let (c, e, r, reply) = (c.clone(), e.clone(), r.clone(), reply.clone());
                async move {
                    c.fetch_add(1, Ordering::SeqCst);
                    e.add_permits(1);
                    r.acquire().await.unwrap().forget();
                    axum::Json(reply)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        Self {
            url,
            payloads,
            calls,
            entered,
            release,
            task,
        }
    }
    fn app(&self, path: &str) -> Router {
        let mut c = config(path);
        c.runtime = Runtime::Dsh {
            url: self.url.clone(),
            token: "fake-internal-token".into(),
        };
        app(Hub::open(c).unwrap())
    }
    async fn wait(&self) {
        self.entered.acquire().await.unwrap().forget();
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}
fn success() -> Value {
    json!({"reply":"fixture reply","backend":"dsh","proposal":null})
}
struct Database(std::path::PathBuf);
impl Database {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!("agenct-turn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&p).unwrap();
        Self(p)
    }
    fn path(&self) -> String {
        self.0.join("hub.db").to_str().unwrap().into()
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn confirmed_task(app: &Router, who: &str, owner: &str, summary: &str) -> Value {
    let (status, proposal) = request(
        app,
        who,
        "POST",
        "/api/proposals",
        json!({"owner":owner,"summary":summary}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, confirmed) = request(
        app,
        who,
        "POST",
        &format!(
            "/api/proposals/{}/actions",
            proposal["id"].as_str().unwrap()
        ),
        json!({"action":"confirm","expected_revision":proposal["revision"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, detail) = request(
        app,
        who,
        "GET",
        &format!("/api/tasks/{}", confirmed["task_id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    detail["task"].clone()
}
fn route(task: &Value) -> String {
    format!("/api/tasks/{}/drafts", task["id"].as_str().unwrap())
}
fn input(key: &str) -> Value {
    json!({"request_id":key,"expected_revision":1})
}
async fn working(a: &Router) -> Value {
    let t = confirmed_task(a, "alice", "bob", "shared summary only").await;
    assert_eq!(
        request(
            a,
            "bob",
            "POST",
            &format!("/api/tasks/{}/actions", t["id"].as_str().unwrap()),
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    t
}
#[tokio::test]
async fn owner_only_context_isolation_atomic_private_completion_and_namespace() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let t = working(&a).await;
    let path = route(&t);
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("INSERT INTO messages(owner,role,content,status) VALUES ('bob','user','BOB_SECRET','complete'),('alice','assistant','ALICE_SECRET','complete')").unwrap();
    let before = request(
        &a,
        "alice",
        "GET",
        &format!("/api/tasks/{}", t["id"].as_str().unwrap()),
        Value::Null,
    )
    .await;
    for who in ["alice", "eve"] {
        assert_eq!(
            request(&a, who, "POST", &path, input("same")).await.0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(&a, who, "GET", &path, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(adapter.count(), 0);
    let first = request(&a, "bob", "POST", &path, input("same")).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["status"], "complete");
    assert_eq!(
        request(&a, "bob", "POST", &path, input("same")).await,
        first
    );
    assert_eq!(adapter.count(), 1);
    let payload = adapter.payloads.lock().unwrap()[0].clone();
    assert_eq!(payload["kind"], "draft_delivery");
    assert_eq!(payload["history"], json!([]));
    assert_eq!(payload["task_context"]["id"], t["id"]);
    assert!(!payload.to_string().contains("SECRET"));
    let ordinary: String = sql
        .query_row(
            "SELECT session_id FROM sessions WHERE owner='bob'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(payload["session_id"], ordinary);
    assert_eq!(
        request(
            &a,
            "alice",
            "GET",
            &format!("/api/tasks/{}", t["id"].as_str().unwrap()),
            Value::Null
        )
        .await,
        before
    );
    for who in ["alice", "eve"] {
        assert_eq!(
            request(&a, who, "GET", &format!("{path}/turns/same"), Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM deliveries WHERE status='draft'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        sql.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            "/api/messages",
            json!({"request_id":"same","message":"independent chat"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(adapter.count(), 2);
    let t2 = working(&a).await;
    assert_eq!(
        request(&a, "bob", "POST", &route(&t2), input("same"))
            .await
            .0,
        StatusCode::CONFLICT
    );
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(
        request(&a, "bob", "POST", &path, input("same")).await,
        first
    );
    assert_eq!(adapter.count(), 2);
}
#[tokio::test]
async fn state_role_input_checks_make_zero_calls_and_conflict_is_not_predispatch() {
    let adapter = Adapter::new(success(), false).await;
    let a = adapter.app(":memory:");
    let t = confirmed_task(&a, "alice", "bob", "task").await;
    let path = route(&t);
    let r = request(
        &a,
        "bob",
        "POST",
        &path,
        json!({"request_id":"state","expected_revision":0}),
    )
    .await;
    assert_eq!(r.1["code"], "draft_task_state_before_reservation");
    request(
        &a,
        "bob",
        "POST",
        &format!("/api/tasks/{}/actions", t["id"].as_str().unwrap()),
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    let stale = request(
        &a,
        "bob",
        "POST",
        &path,
        json!({"request_id":"stale","expected_revision":0}),
    )
    .await;
    assert_eq!(stale.1["code"], "draft_task_stale_before_reservation");
    for body in [
        json!({"request_id":"bad","expected_revision":1,"history":[]}),
        json!({"request_id":"bad","expected_revision":-1}),
        json!({"request_id":"bad","expected_revision":1,"agent_id":"butler"}),
        json!({"request_id":"bad","expected_revision":1,"expected_profile_revision":0}),
    ] {
        assert!(request(&a, "bob", "POST", &path, body)
            .await
            .0
            .is_client_error());
    }
    let role=request(&a,"bob","POST",&path,json!({"request_id":"role","expected_revision":1,"agent_id":"missing","expected_profile_revision":0})).await;
    assert_eq!(role.1["code"], "draft_role_unavailable_before_reservation");
    assert_eq!(adapter.count(), 0);
    let first = request(&a, "bob", "POST", &path, input("ok")).await;
    assert_eq!(first.0, StatusCode::OK);
    let conflict = request(
        &a,
        "bob",
        "POST",
        &path,
        json!({"request_id":"ok","expected_revision":2}),
    )
    .await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert!(conflict.1.get("code").is_none());
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn concurrent_replay_and_task_change_retain_stale_private_output() {
    let adapter = Adapter::new(success(), true).await;
    let a = adapter.app(":memory:");
    let t = working(&a).await;
    let path = route(&t);
    let (clone, p) = (a.clone(), path.clone());
    let run = tokio::spawn(async move { request(&clone, "bob", "POST", &p, input("one")).await });
    adapter.wait().await;
    assert_eq!(
        request(&a, "bob", "POST", &path, input("one")).await.1["status"],
        "pending"
    );
    let busy = request(&a, "bob", "POST", &path, input("two")).await;
    assert_eq!(busy.1["code"], "draft_busy_before_reservation");
    assert_eq!(adapter.count(), 1);
    request(
        &a,
        "alice",
        "POST",
        &format!("/api/tasks/{}/actions", t["id"].as_str().unwrap()),
        json!({"action":"cancel","expected_revision":1,"reason":"This task is no longer needed"}),
    )
    .await;
    adapter.release.add_permits(1);
    let first = run.await.unwrap();
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["response"]["draft"]["stale"], true);
    assert_eq!(request(&a, "bob", "POST", &path, input("one")).await, first);
    assert_eq!(
        request(&a, "bob", "GET", &path, Value::Null).await.1["items"][0]["current_stale"],
        true
    );
    assert_eq!(
        request(&a, "alice", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn malformed_outputs_and_cancellation_never_redispatch() {
    for reply in [
        json!({"reply":"draft","backend":"dsh","proposal":{"owner":"alice","summary":"shared"}}),
        json!({"reply":"draft","backend":"mock","proposal":null}),
        json!({"reply":"x".repeat(8001),"backend":"dsh","proposal":null}),
        json!({"reply":"draft","backend":"dsh"}),
    ] {
        let adapter = Adapter::new(reply, false).await;
        let a = adapter.app(":memory:");
        let t = working(&a).await;
        let path = route(&t);
        assert_eq!(
            request(&a, "bob", "POST", &path, input("one")).await.0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            request(&a, "bob", "POST", &path, input("one")).await.1["status"],
            "uncertain"
        );
        assert_eq!(adapter.count(), 1);
    }
    let adapter = Adapter::new(success(), true).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let t = working(&a).await;
    let path = route(&t);
    let (clone, p) = (a.clone(), path.clone());
    let run =
        tokio::spawn(async move { request(&clone, "bob", "POST", &p, input("abandoned")).await });
    adapter.wait().await;
    run.abort();
    let _ = run.await;
    assert_eq!(
        request(
            &a,
            "bob",
            "GET",
            &format!("{path}/turns/abandoned"),
            Value::Null
        )
        .await
        .1["status"],
        "uncertain"
    );
    assert_eq!(
        request(&a, "bob", "POST", &path, input("abandoned"))
            .await
            .1["status"],
        "uncertain"
    );
    let (clone, p) = (a.clone(), path.clone());
    let other =
        tokio::spawn(
            async move { request(&clone, "bob", "POST", &p, input("new-after-abort")).await },
        );
    adapter.wait().await;
    assert_eq!(
        request(
            &a,
            "bob",
            "GET",
            &format!("{path}/turns/abandoned"),
            Value::Null
        )
        .await
        .1["status"],
        "uncertain"
    );
    other.abort();
    let _ = other.await;
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(
        request(&a, "bob", "POST", &path, input("abandoned"))
            .await
            .1["status"],
        "uncertain"
    );
    assert_eq!(adapter.count(), 2);
}
#[tokio::test]
async fn reservation_and_completion_failures_are_atomic_and_receipts_cannot_be_replaced() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let t = working(&a).await;
    let path = route(&t);
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER reserve_fail BEFORE INSERT ON draft_turns BEGIN SELECT RAISE(ABORT,'test'); END").unwrap();
    assert_eq!(
        request(&a, "bob", "POST", &path, input("reserve")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(adapter.count(), 0);
    sql.execute_batch("DROP TRIGGER reserve_fail").unwrap();
    sql.execute_batch("CREATE TRIGGER finish_fail BEFORE UPDATE ON draft_turns WHEN NEW.status='complete' BEGIN SELECT RAISE(ABORT,'test'); END").unwrap();
    assert_eq!(
        request(&a, "bob", "POST", &path, input("finish")).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(adapter.count(), 1);
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM deliveries WHERE status='draft'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        request(&a, "bob", "POST", &path, input("finish")).await.1["status"],
        "uncertain"
    );
    sql.execute_batch("DROP TRIGGER finish_fail").unwrap();
    assert!(sql
        .execute_batch("UPDATE draft_turns SET binding='{}'")
        .is_err());
    assert!(sql.execute_batch("DELETE FROM draft_turns").is_err());
    assert!(sql
        .execute_batch("INSERT OR REPLACE INTO draft_turns SELECT * FROM draft_turns")
        .is_err());
    assert_eq!(
        request(&a, "bob", "POST", &path, input("finish")).await.1["status"],
        "uncertain"
    );
    assert_eq!(
        request(&a, "bob", "POST", &path, input("done")).await.0,
        StatusCode::OK
    );
    sql.execute_batch("INSERT INTO draft_turns(owner,request_id,task_id,binding,status,task_context,runtime_profile,session_id) SELECT owner,'reserved',task_id,binding,'pending',task_context,runtime_profile,session_id FROM draft_turns WHERE request_id='done'").unwrap();
    assert!(sql.execute_batch("PRAGMA recursive_triggers=OFF; UPDATE OR REPLACE draft_turns SET status='complete',response=(SELECT response FROM draft_turns WHERE request_id='done'),draft_id=(SELECT draft_id FROM draft_turns WHERE request_id='done') WHERE request_id='reserved'").is_err());
    assert_eq!(
        sql.query_row(
            "SELECT count(*) FROM draft_turns WHERE request_id IN ('done','reserved')",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        2
    );
}

#[tokio::test]
async fn owner_discovery_is_paginated_and_mock_never_submits() {
    let a = app(Hub::open(config(":memory:")).unwrap());
    let t = working(&a).await;
    let path = route(&t);
    for key in ["a", "b", "c"] {
        assert_eq!(
            request(&a, "bob", "POST", &path, input(key)).await.0,
            StatusCode::OK
        );
    }
    let page = request(&a, "bob", "GET", &format!("{path}?limit=2"), Value::Null)
        .await
        .1;
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["next_cursor"], "b");
    let last = request(
        &a,
        "bob",
        "GET",
        &format!("{path}?limit=2&after=b"),
        Value::Null,
    )
    .await
    .1;
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert!(last["next_cursor"].is_null());
    assert!(
        request(&a, "bob", "GET", &format!("{path}?limit=100"), Value::Null)
            .await
            .0
            .is_client_error()
    );
    let shared = request(
        &a,
        "alice",
        "GET",
        &format!("/api/tasks/{}", t["id"].as_str().unwrap()),
        Value::Null,
    )
    .await
    .1;
    assert_eq!(shared["deliveries"], json!([]));
    assert_eq!(shared["task"]["revision"], 1);
    assert_eq!(shared["task"]["status"], "working");
    assert_eq!(
        request(&a, "bob", "GET", "/api/messages", Value::Null)
            .await
            .1,
        json!([])
    );
}

#[tokio::test]
async fn historical_replay_ignores_new_profile_and_another_active_turn() {
    let adapter = Adapter::new(success(), true).await;
    let a = adapter.app(":memory:");
    let t = working(&a).await;
    let path = route(&t);
    adapter.release.add_permits(1);
    let first = request(&a, "bob", "POST", &path, input("first")).await;
    assert_eq!(first.0, StatusCode::OK);
    adapter.wait().await;
    let profile = request(&a, "bob", "GET", "/api/profile", Value::Null)
        .await
        .1;
    let mut changed = profile.clone();
    changed["agents"][0]["name"] = json!("Private renamed role");
    let profile_write = request(
        &a,
        "bob",
        "PUT",
        "/api/profile",
        json!({"expected_revision":profile["revision"],"agents":changed["agents"]}),
    )
    .await;
    assert_eq!(profile_write.0, StatusCode::OK);
    let stale_role=request(&a,"bob","POST",&path,json!({"request_id":"stale-role","expected_revision":1,"agent_id":profile["agents"][0]["id"],"expected_profile_revision":profile["revision"]})).await;
    assert_eq!(
        stale_role.1["code"],
        "draft_role_unavailable_before_reservation"
    );
    let (clone, p) = (a.clone(), path.clone());
    let second =
        tokio::spawn(async move { request(&clone, "bob", "POST", &p, input("second")).await });
    adapter.wait().await;
    assert_eq!(
        request(&a, "bob", "POST", &path, input("first")).await,
        first
    );
    assert_eq!(adapter.count(), 2);
    adapter.release.add_permits(1);
    assert_eq!(second.await.unwrap().0, StatusCode::OK);
    request(
        &a,
        "bob",
        "POST",
        &format!("/api/tasks/{}/actions", t["id"].as_str().unwrap()),
        json!({"action":"deliver","expected_revision":1,"result":"human reviewed public text"}),
    )
    .await;
    let replay = request(&a, "bob", "POST", &path, input("first")).await;
    assert_eq!(replay.1["response"], first.1["response"]);
    assert_eq!(replay.1["current_stale"], true);
    let lookup = request(
        &a,
        "bob",
        "GET",
        &format!("{path}/turns/first"),
        Value::Null,
    )
    .await;
    assert_eq!(lookup.1["response"], first.1["response"]);
    assert_eq!(lookup.1["current_stale"], true);

    let list = request(&a, "bob", "GET", &path, Value::Null).await.1;
    assert_eq!(list["items"][0]["current_stale"], true);
    let state = request(
        &a,
        "bob",
        "POST",
        &path,
        json!({"request_id":"ready","expected_revision":2}),
    )
    .await;
    assert_eq!(state.1["code"], "draft_task_state_before_reservation");
    let public = request(
        &a,
        "alice",
        "GET",
        &format!("/api/tasks/{}", t["id"].as_str().unwrap()),
        Value::Null,
    )
    .await
    .1;
    assert_eq!(public["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(public["deliveries"][0]["seq"], 1);
    assert_eq!(
        public["deliveries"][0]["content"],
        "human reviewed public text"
    );
    assert!(!public.to_string().contains("fixture reply"));
}

#[tokio::test]
async fn revision_is_private_version_bound_and_replay_does_not_call_model_twice() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let task = working(&a).await;
    let path = route(&task);
    let first = request(&a, "bob", "POST", &path, input("original")).await;
    assert_eq!(first.0, StatusCode::OK);
    let base = first.1["response"]["draft"]["id"].clone();
    let mut revision = input("revision");
    revision["revision"] = json!({"base_draft_id":base,"instruction":"Shorter, keep evidence"});
    let changed = request(&a, "bob", "POST", &path, revision.clone()).await;
    assert_eq!(changed.0, StatusCode::OK);
    assert_eq!(changed.1["response"]["draft"]["seq"], 2);
    assert_eq!(changed.1["response"]["draft"]["base_draft_id"], base);
    assert_eq!(
        request(&a, "bob", "POST", &path, revision.clone()).await.1,
        changed.1
    );
    assert_eq!(adapter.count(), 2);
    {
        let payloads = adapter.payloads.lock().unwrap();
        assert!(payloads[0].get("draft_revision").is_none());
        assert_eq!(
            payloads[1]["draft_revision"],
            json!({"base_draft_id":base,"content":"fixture reply","instruction":"Shorter, keep evidence"})
        );
        assert_eq!(payloads[1]["history"], json!([]));
    }
    revision["revision"]["instruction"] = json!("Different instruction");
    assert_eq!(
        request(&a, "bob", "POST", &path, revision.clone()).await.0,
        StatusCode::CONFLICT
    );
    revision["request_id"] = json!("cross-owner");
    assert_eq!(
        request(&a, "alice", "POST", &path, revision.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let other = working(&a).await;
    assert_eq!(
        request(&a, "bob", "POST", &route(&other), revision.clone())
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute(
        "UPDATE tasks SET revision=revision+1 WHERE id=?1",
        [task["id"].as_str().unwrap()],
    )
    .unwrap();
    revision["expected_revision"] = json!(2);
    assert_eq!(
        request(&a, "bob", "POST", &path, revision).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(adapter.count(), 2);
    let list = request(&a, "bob", "GET", &path, Value::Null).await.1;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    assert!(list["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["current_stale"] == true));
}

#[tokio::test]
async fn draft_recommendation_uses_only_enabled_personal_roles() {
    let a = app(Hub::open(config(":memory:")).unwrap());
    let task = working(&a).await;
    let agents = json!([
      {"id":"butler","name":"管家","role":"coordinator","model_ref":"server-default","enabled":true},
      {"id":"writer","name":"写作","role":"writer","model_ref":"server-default","enabled":true},
      {"id":"reviewer","name":"审阅","role":"reviewer","model_ref":"server-default","enabled":false}
    ]);
    assert_eq!(
        request(
            &a,
            "bob",
            "PUT",
            "/api/profile",
            json!({"expected_revision":0,"agents":agents})
        )
        .await
        .0,
        StatusCode::OK
    );
    let list = request(&a, "bob", "GET", &route(&task), Value::Null)
        .await
        .1;
    assert_eq!(list["recommended_profile"]["agent_id"], "writer");
    assert_eq!(list["recommended_profile"]["revision"], 1);
    assert_eq!(list["items"], json!([]));
}

#[tokio::test]
async fn summary_failure_is_reserved_and_never_rebilled_on_draft_replay() {
    let adapter = Adapter::new(json!({"reply":"", "proposal":null, "backend":"dsh"}), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let task = working(&a).await;
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute(
        "INSERT INTO task_details(task_id,author,content) VALUES (?1,'alice',?2)",
        rusqlite::params![task["id"].as_str().unwrap(), "long material ".repeat(1500)],
    )
    .unwrap();
    let path = route(&task);
    assert_eq!(
        request(&a, "bob", "POST", &path, input("large-failed"))
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
    let replay = request(&a, "bob", "POST", &path, input("large-failed")).await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["status"], "uncertain");
    assert_eq!(adapter.count(), 1);
    assert_eq!(
        sql.query_row("SELECT count(*) FROM deliveries", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
