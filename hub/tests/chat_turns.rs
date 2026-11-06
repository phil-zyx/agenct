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

fn selected_agent(key: &str, agent: &str, revision: i64) -> Value {
    json!({"request_id":key,"message":"review this","agent_id":agent,"expected_profile_revision":revision})
}
fn role_profile(revision: i64, enabled: bool) -> Value {
    json!({"expected_revision":revision,"agents":[
        {"id":"butler","name":"Coordinator","role":"coordinator","model_ref":"server-default","enabled":true},
        {"id":"research","name":"Research","role":"researcher","model_ref":"server-default","enabled":enabled}
    ]})
}
#[tokio::test]
async fn explicit_selection_rejects_stale_disabled_unknown_and_other_owner_before_reservation() {
    let a = app(Hub::open(config(":memory:")).unwrap());
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", role_profile(0, false))
            .await
            .0,
        StatusCode::OK
    );
    for (who, agent, revision, code, status) in [
        (
            "alice",
            "butler",
            0,
            "selected_agent_stale_before_dispatch",
            StatusCode::CONFLICT,
        ),
        (
            "alice",
            "research",
            1,
            "selected_agent_unavailable_before_dispatch",
            StatusCode::BAD_REQUEST,
        ),
        (
            "alice",
            "missing",
            1,
            "selected_agent_unavailable_before_dispatch",
            StatusCode::BAD_REQUEST,
        ),
        (
            "bob",
            "research",
            0,
            "selected_agent_unavailable_before_dispatch",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let response = request(
            &a,
            who,
            "POST",
            "/api/messages",
            selected_agent("rejected", agent, revision),
        )
        .await;
        assert_eq!(response.0, status);
        assert_eq!(response.1["code"], code);
        assert_eq!(
            request(&a, who, "GET", "/api/turns/rejected", Value::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            request(&a, who, "GET", "/api/messages", Value::Null)
                .await
                .1,
            json!([])
        );
    }
    // Rejected selections did not consume the ID. Owner revision is independent.
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            "/api/messages",
            selected_agent("rejected", "butler", 0)
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            selected_agent("rejected", "butler", 1)
        )
        .await
        .0,
        StatusCode::OK
    );
    // Existing clients may still omit the revision.
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"legacy","agent_id":"butler"})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn malformed_explicit_selection_never_reserves_a_receipt() {
    let a = app(Hub::open(config(":memory:")).unwrap());
    for patch in [
        json!({"expected_profile_revision":0}),
        json!({"agent_id":"butler","expected_profile_revision":-1}),
        json!({"agent_id":"","expected_profile_revision":0}),
        json!({"agent_id":"bad/id","expected_profile_revision":0}),
        json!({"agent_id":"x".repeat(33),"expected_profile_revision":0}),
        json!({"agent_id":"butler","expected_profile_revision":"0"}),
        json!({"agent_id":"butler","expected_profile_revision":0.5}),
        json!({"agent_id":"butler","expected_profile_revision":18446744073709551615u64}),
    ] {
        let mut body = turn("malformed");
        body.as_object_mut()
            .unwrap()
            .extend(patch.as_object().unwrap().clone());
        assert!(request(&a, "alice", "POST", "/api/messages", body)
            .await
            .0
            .is_client_error());
        assert_eq!(lookup(&a, "malformed").await.0, StatusCode::NOT_FOUND);
    }
    assert_eq!(
        request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1,
        json!([])
    );
}
#[tokio::test]
async fn explicit_revision_is_frozen_during_edit_and_bound_to_receipt_after_restart() {
    let adapter = Adapter::new(success(), true).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", role_profile(0, true))
            .await
            .0,
        StatusCode::OK
    );
    let body = selected_agent("selected", "research", 1);
    let running = a.clone();
    let original = body.clone();
    let task =
        tokio::spawn(
            async move { request(&running, "alice", "POST", "/api/messages", original).await },
        );
    adapter.wait().await;
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", role_profile(1, false))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body.clone())
            .await
            .1["turn"]["status"],
        "pending"
    );
    let changed = request(
        &a,
        "alice",
        "POST",
        "/api/messages",
        selected_agent("selected", "research", 2),
    )
    .await;
    assert_eq!(changed.0, StatusCode::CONFLICT);
    assert!(changed.1.get("code").is_none());
    adapter.release.add_permits(1);
    let completed = task.await.unwrap();
    assert_eq!(completed.0, StatusCode::OK);
    assert_eq!(completed.1["runtime_profile"]["revision"], 1);
    assert_eq!(completed.1["runtime_profile"]["agent_id"], "research");
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body.clone()).await,
        completed
    );
    drop(a);
    let a = adapter.app(&db.path());
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", body).await,
        completed
    );
    let mut legacy = selected_agent("selected", "research", 1);
    legacy
        .as_object_mut()
        .unwrap()
        .remove("expected_profile_revision");
    let conflict = request(&a, "alice", "POST", "/api/messages", legacy).await;
    assert_eq!(conflict.0, StatusCode::CONFLICT);
    assert!(conflict.1.get("code").is_none());
    assert_eq!(adapter.count(), 1);
}
#[tokio::test]
async fn old_receipt_schema_migrates_revision_without_rebinding_legacy_turns() {
    let db = Database::new();
    let sql = rusqlite::Connection::open(db.path()).unwrap();
    sql.execute_batch("CREATE TABLE chat_turns(owner TEXT NOT NULL,request_id TEXT NOT NULL,message TEXT NOT NULL,agent_id TEXT,runtime_profile TEXT NOT NULL,status TEXT NOT NULL,response TEXT,message_id INTEGER,PRIMARY KEY(owner,request_id));").unwrap();
    let original =
        json!({"reply":"frozen legacy","turn":{"request_id":"legacy","status":"complete"}});
    sql.execute("INSERT INTO chat_turns(owner,request_id,message,agent_id,runtime_profile,status,response) VALUES ('alice','legacy','review this','butler','{}','complete',?1)", [original.to_string()]).unwrap();
    drop(sql);
    for _ in 0..2 {
        let a = app(Hub::open(config(&db.path())).unwrap());
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"request_id":"legacy","message":"review this","agent_id":"butler"})
            )
            .await
            .1,
            original
        );
        let conflict = request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            selected_agent("legacy", "butler", 0),
        )
        .await;
        assert_eq!(conflict.0, StatusCode::CONFLICT);
        assert!(conflict.1.get("code").is_none());
    }
}

#[tokio::test]
async fn unavailable_recipient_becomes_hub_notice_without_proposal() {
    for (owner, expected) in [
        ("carol", "「carol」不是本系统的参与者"),
        ("alice", "不能把任务委派给你自己"),
        ("<script>", "指定的接收人不是本系统的参与者"),
    ] {
        let reply = json!({"reply":"我先起草一条给对方的请求","backend":"dsh",
            "proposal":{"owner":owner,"summary":"整理会议纪要"}});
        let adapter = Adapter::new(reply, false).await;
        let db = Database::new();
        let a = adapter.app(&db.path());
        let message = format!("让 {owner} 帮我整理会议纪要");
        let (status, body) = request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            json!({"request_id":"r1","message":message}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{owner}: {body}");
        let text = body["reply"].as_str().unwrap();
        assert!(
            text.starts_with("[系统提示]") && text.contains(expected),
            "{text}"
        );
        assert!(text.contains("可委派给：bob、eve"), "{text}");
        assert!(!text.contains("我先起草"), "model reply must not be shown");
        assert!(body["proposal"].is_null());
        assert_eq!(
            request(&a, "alice", "GET", "/api/proposals", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            request(&a, "bob", "GET", "/api/tasks", Value::Null).await.1,
            json!([])
        );
        // The durable receipt replays the same notice instead of redispatching.
        assert_eq!(send_key(&a, "r1", &message).await.1["reply"], body["reply"]);
        assert_eq!(adapter.count(), 1);
        let history = request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1;
        assert_eq!(
            history.as_array().unwrap().last().unwrap()["content"],
            body["reply"]
        );
    }
}
async fn send_key(a: &Router, key: &str, message: &str) -> (StatusCode, Value) {
    request(
        a,
        "alice",
        "POST",
        "/api/messages",
        json!({"request_id":key,"message":message}),
    )
    .await
}

#[tokio::test]
async fn unnamed_invalid_recipient_still_fails_closed() {
    // The user named nobody invalid; the model invented "carol". Protocol violation.
    let reply =
        json!({"reply":"ok","backend":"dsh","proposal":{"owner":"carol","summary":"整理会议纪要"}});
    let adapter = Adapter::new(reply, false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let (status, _) = request(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({"request_id":"r1","message":"帮我整理会议纪要"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        request(&a, "alice", "GET", "/api/proposals", Value::Null)
            .await
            .1,
        json!([])
    );
}

#[tokio::test]
async fn other_invalid_proposals_still_fail_closed() {
    // A configured recipient with a summary that is not an excerpt is a protocol
    // violation, not a recipient problem: keep the 502 and persist nothing.
    let reply =
        json!({"reply":"ok","backend":"dsh","proposal":{"owner":"bob","summary":"invented text"}});
    let adapter = Adapter::new(reply, false).await;
    let db = Database::new();
    let a = adapter.app(&db.path());
    let (status, _) = request(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({"request_id":"r1","message":"让 bob 帮我看看"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        request(&a, "alice", "GET", "/api/proposals", Value::Null)
            .await
            .1,
        json!([])
    );
}
