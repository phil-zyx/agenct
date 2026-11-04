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
        let calls = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Semaphore::new(0));
        let release = Arc::new(Semaphore::new(if blocked { 0 } else { 100 }));
        let (c, e, r) = (calls.clone(), entered.clone(), release.clone());
        let adapter = Router::new().route(
            "/v1/turn",
            axum::routing::post(move || {
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
fn turn(key: &str) -> Value {
    json!({"request_id":key,"message":"review this"})
}
async fn send(a: &Router, key: &str) -> (StatusCode, Value) {
    request(a, "alice", "POST", "/api/messages", turn(key)).await
}
async fn lookup(a: &Router, key: &str) -> (StatusCode, Value) {
    request(a, "alice", "GET", &format!("/api/turns/{key}"), Value::Null).await
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

#[tokio::test]
async fn completed_receipt_replays_once_and_survives_restart() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let first = send(&a, "one").await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(first.1["turn"]["status"], "complete");
    assert_eq!(send(&a, "one").await, first);
    assert_eq!(adapter.count(), 1);
    assert_eq!(lookup(&a, "one").await.1["response"], first.1);
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        2
    );
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(send(&a, "one").await, first);
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn owner_isolation_changed_content_and_agent_conflict_before_dispatch() {
    let adapter = Adapter::new(success(), false).await;
    let a = adapter.app(":memory:");
    send(&a, "one").await;
    for field in ["message", "agent_id"] {
        let mut input = turn("one");
        input[field] = json!("changed");
        assert_eq!(
            request(&a, "alice", "POST", "/api/messages", input).await.0,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        request(&a, "bob", "GET", "/api/turns/one", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&a, "mallory", "GET", "/api/turns/one", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(adapter.count(), 1);
    assert_eq!(
        request(&a, "bob", "POST", "/api/messages", turn("one"))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(adapter.count(), 2); // IDs are private to each authenticated owner.
}
#[tokio::test]
async fn concurrent_identical_key_returns_pending_without_second_dispatch() {
    let adapter = Adapter::new(success(), true).await;
    let a = adapter.app(":memory:");
    let running = a.clone();
    let task = tokio::spawn(async move { send(&running, "one").await });
    adapter.wait().await;
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "pending");
    assert_eq!(lookup(&a, "one").await.1["status"], "pending");
    assert_eq!(send(&a, "two").await.0, StatusCode::CONFLICT);
    assert_eq!(adapter.count(), 1);
    adapter.release.add_permits(1);
    let completed = task.await.unwrap();
    assert_eq!(send(&a, "one").await, completed);
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn malformed_provider_result_is_uncertain_and_never_redispatched() {
    let adapter = Adapter::new(json!({"reply":"malformed"}), false).await;
    let a = adapter.app(":memory:");
    assert_eq!(send(&a, "one").await.0, StatusCode::BAD_GATEWAY);
    assert_eq!(lookup(&a, "one").await.1["status"], "uncertain");
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1[0]["status"],
        "uncertain"
    );
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "uncertain");
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn abandoned_dispatch_remains_uncertain_in_process_and_after_restart() {
    let adapter = Adapter::new(success(), true).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let running = a.clone();
    let task = tokio::spawn(async move { send(&running, "one").await });
    adapter.wait().await;
    task.abort();
    let _ = task.await;
    assert_eq!(lookup(&a, "one").await.1["status"], "uncertain");
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1[0]["status"],
        "uncertain"
    );
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "uncertain");
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "uncertain");
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn restart_converts_unobserved_pending_to_uncertain() {
    let adapter = Adapter::new(success(), true).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let running = a.clone();
    let task = tokio::spawn(async move { send(&running, "one").await });
    adapter.wait().await;
    task.abort();
    let _ = task.await;
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(lookup(&a, "one").await.1["status"], "uncertain");
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1[0]["status"],
        "uncertain"
    );
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "uncertain");
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn atomic_storage_failure_rolls_back_reply_and_does_not_repeat_adapter() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE OF response ON chat_turns WHEN NEW.status='complete' BEGIN SELECT RAISE(ABORT,'test storage failure'); END;").unwrap();
    assert_eq!(send(&a, "one").await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(lookup(&a, "one").await.1["status"], "uncertain");
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1[0]["status"],
        "uncertain"
    );
    let history = request(&a, "alice", "GET", "/api/messages", Value::Null)
        .await
        .1;
    assert_eq!(history.as_array().unwrap().len(), 1);
    assert_eq!(history[0]["role"], "user");
    assert_eq!(history[0]["status"], "uncertain");
    sql.execute_batch("DROP TRIGGER fail_completion").unwrap();
    assert_eq!(send(&a, "one").await.1["turn"]["status"], "uncertain");
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn profile_edits_do_not_change_inflight_or_replayed_profile() {
    let adapter = Adapter::new(success(), true).await;
    let a = adapter.app(":memory:");
    let running = a.clone();
    let task = tokio::spawn(async move { send(&running, "one").await });
    adapter.wait().await;
    let update = json!({"expected_revision":0,"agents":[{"id":"newcoordinator","name":"New","role":"coordinator","model_ref":"server-default","enabled":true}]});
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", update).await.0,
        StatusCode::OK
    );
    adapter.release.add_permits(1);
    let completed = task.await.unwrap();
    assert_eq!(completed.1["runtime_profile"]["revision"], 0);
    assert_eq!(send(&a, "one").await, completed);
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn proposal_finalization_is_atomic_private_and_replayed_once() {
    let adapter=Adapter::new(json!({"reply":"confirm","backend":"dsh","proposal":{"owner":"bob","summary":"review this"}}),false).await;
    let a = adapter.app(":memory:");
    let body = json!({"message":"bob review this","request_id":"one"});
    let first = request(&a, "alice", "POST", "/api/messages", body.clone()).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body).await,
        first
    );
    assert_eq!(
        request(&a, "alice", "GET", "/api/proposals", Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(request(&a, "bob", "GET", "/api/tasks", Value::Null)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn cross_endpoint_request_ids_conflict_and_legacy_chat_still_works() {
    let adapter = Adapter::new(success(), false).await;
    let a = adapter.app(":memory:");
    let proposal = json!({"owner":"bob","summary":"review this","request_id":"proposal"});
    assert_eq!(
        request(&a, "alice", "POST", "/api/proposals", proposal.clone())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(send(&a, "proposal").await.0, StatusCode::CONFLICT);
    assert_eq!(adapter.count(), 0);
    send(&a, "chat").await;
    let mut other = proposal;
    other["request_id"] = json!("chat");
    assert_eq!(
        request(&a, "alice", "POST", "/api/proposals", other)
            .await
            .0,
        StatusCode::CONFLICT
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"legacy"})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(adapter.count(), 3);
}

#[tokio::test]
async fn failed_reservation_never_dispatches_or_creates_messages() {
    let adapter = Adapter::new(success(), false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_reservation BEFORE INSERT ON chat_turns BEGIN SELECT RAISE(ABORT,'test storage failure'); END;").unwrap();
    assert_eq!(send(&a, "one").await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(adapter.count(), 0);
    assert_eq!(lookup(&a, "one").await.0, StatusCode::NOT_FOUND);
    assert!(request(&a, "alice", "GET", "/api/messages", Value::Null)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
}
#[tokio::test]
async fn completed_replay_works_while_another_turn_holds_owner_gate() {
    let adapter = Adapter::new(success(), true).await;
    adapter.release.add_permits(1);
    let a = adapter.app(":memory:");
    let first = send(&a, "one").await;
    adapter.wait().await;
    let running = a.clone();
    let task = tokio::spawn(async move { send(&running, "two").await });
    adapter.wait().await;
    assert_eq!(send(&a, "one").await, first);
    assert_eq!(adapter.count(), 2);
    adapter.release.add_permits(1);
    assert_eq!(task.await.unwrap().0, StatusCode::OK);
}
#[tokio::test]
async fn completion_failure_rolls_back_private_proposal_as_well_as_reply() {
    let adapter=Adapter::new(json!({"reply":"confirm","backend":"dsh","proposal":{"owner":"bob","summary":"review this"}}),false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE OF response ON chat_turns WHEN NEW.status='complete' BEGIN SELECT RAISE(ABORT,'test storage failure'); END;").unwrap();
    let body = json!({"message":"bob review this","request_id":"one"});
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body.clone())
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(lookup(&a, "one").await.1["status"], "uncertain");
    assert!(request(&a, "alice", "GET", "/api/proposals", Value::Null)
        .await
        .1
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body).await.1["turn"]["status"],
        "uncertain"
    );
    assert_eq!(adapter.count(), 1);
}
