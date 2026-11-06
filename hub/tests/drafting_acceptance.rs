//! Offline application acceptance using the explicitly labeled MockRuntime.
//! No model, adapter server, browser, executor or publication is invoked.
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

struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("agenct-draft-acceptance-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> String {
        self.0.join("hub.db").to_str().unwrap().into()
    }
    fn config(&self) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: self.path().into(),
            runtime: Runtime::Mock,
            users: ["alice", "bob", "eve"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn token(who: &str) -> String {
    format!("fixture-{who}-{}", "x".repeat(40))
}
async fn request(
    a: &Router,
    who: &str,
    method: &str,
    path: &str,
    data: Value,
) -> (StatusCode, Value) {
    let response = a
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
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn ok(a: &Router, who: &str, method: &str, path: &str, data: Value) -> Value {
    let (status, body) = request(a, who, method, path, data).await;
    assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
    body
}
fn count(db: &rusqlite::Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |r| r.get(0)).unwrap()
}
fn no_execution(db: &rusqlite::Connection) {
    assert_eq!(count(db, "SELECT count(*) FROM worker_jobs"), 0);
    assert_eq!(
        count(
            db,
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name LIKE 'publication%'"
        ),
        0
    );
}

#[tokio::test]
async fn mock_recipient_drafting_requires_every_human_handoff_and_keeps_draft_private() {
    let fixture = Fixture::new();
    let a = app(Hub::open(fixture.config()).unwrap());
    let db = rusqlite::Connection::open(fixture.path()).unwrap();
    let chat = ok(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({
            "message":"ask bob to draft release notes", "request_id":"acceptance-proposal"
        }),
    )
    .await;
    assert_eq!(chat["backend"], "mock");
    let proposal = &chat["proposal"];
    assert_eq!(proposal["owner"], "bob");
    assert_eq!(proposal["status"], "pending");
    for who in ["alice", "bob", "eve"] {
        assert_eq!(
            ok(&a, who, "GET", "/api/tasks", Value::Null).await,
            json!([])
        );
    }
    assert_eq!(
        ok(&a, "bob", "GET", "/api/proposals", Value::Null).await,
        json!([])
    );
    no_execution(&db);
    let confirmation = format!(
        "/api/proposals/{}/actions",
        proposal["id"].as_str().unwrap()
    );
    assert_ne!(
        request(
            &a,
            "bob",
            "POST",
            &confirmation,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    let confirmed = ok(
        &a,
        "alice",
        "POST",
        &confirmation,
        json!({"action":"confirm","expected_revision":proposal["revision"]}),
    )
    .await;
    let path = format!("/api/tasks/{}", confirmed["task_id"].as_str().unwrap());
    let actions = format!("{path}/actions");
    let drafts = format!("{path}/drafts");
    let receipt_path = format!("{drafts}/turns/acceptance-draft");
    let initial = ok(&a, "bob", "GET", &path, Value::Null).await;
    assert_eq!(initial["task"]["status"], "awaiting_owner");
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &drafts,
            json!({"request_id":"too-early","expected_revision":0})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(count(&db, "SELECT count(*) FROM draft_turns"), 0);
    ok(
        &a,
        "bob",
        "POST",
        &actions,
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    let before = ok(&a, "alice", "GET", &path, Value::Null).await;
    let messages_before = count(&db, "SELECT count(*) FROM messages");
    let receipt = ok(
        &a,
        "bob",
        "POST",
        &drafts,
        json!({"request_id":"acceptance-draft","expected_revision":1}),
    )
    .await;
    assert_eq!(receipt["status"], "complete");
    assert_eq!(receipt["response"]["backend"], "mock");
    let generated = receipt["response"]["draft"]["content"].as_str().unwrap();
    assert!(generated.starts_with("[MOCK · draft only]"));
    assert_eq!(receipt["current_stale"], false);
    assert_eq!(ok(&a, "alice", "GET", &path, Value::Null).await, before);
    assert_eq!(count(&db, "SELECT count(*) FROM messages"), messages_before);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM deliveries WHERE status='submitted'"
        ),
        0
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM deliveries WHERE status='draft'"),
        1
    );
    for who in ["alice", "eve"] {
        for endpoint in [&drafts, &receipt_path] {
            assert_eq!(
                request(&a, who, "GET", endpoint, Value::Null).await.0,
                StatusCode::NOT_FOUND
            );
        }
        assert_eq!(
            request(
                &a,
                who,
                "POST",
                &drafts,
                json!({"request_id":"intruder","expected_revision":1})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        ok(
            &a,
            "bob",
            "POST",
            &drafts,
            json!({"request_id":"acceptance-draft","expected_revision":1})
        )
        .await,
        receipt
    );
    no_execution(&db);

    // A local human edit alone has no server side effects. Approval cannot skip delivery.
    let reviewed = "Human-reviewed release notes: documented changes only; not execution evidence.";
    assert_ne!(reviewed, generated);
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &actions,
            json!({"action":"approve","expected_revision":1})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(ok(&a, "alice", "GET", &path, Value::Null).await, before);
    ok(
        &a,
        "bob",
        "POST",
        &actions,
        json!({"action":"deliver","expected_revision":1,"result":reviewed}),
    )
    .await;
    let delivered = ok(&a, "alice", "GET", &path, Value::Null).await;
    assert_eq!(delivered["task"]["status"], "ready_for_acceptance");
    assert_eq!(delivered["task"]["result"], reviewed);
    assert_eq!(delivered["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(delivered["deliveries"][0]["seq"], 1);
    assert_eq!(delivered["deliveries"][0]["content"], reviewed);
    assert_eq!(delivered["timeline"]["deliveries"], delivered["deliveries"]);
    assert_eq!(delivered["timeline"]["events"], delivered["events"]);
    assert!(!delivered.to_string().contains(generated));
    assert!(!delivered
        .to_string()
        .contains(receipt["response"]["draft"]["id"].as_str().unwrap()));
    let stored: String = db
        .query_row(
            "SELECT content FROM deliveries WHERE status='draft'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stored, generated);
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM deliveries WHERE status='submitted'"
        ),
        1
    );
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &actions,
            json!({"action":"approve","expected_revision":2})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    ok(
        &a,
        "alice",
        "POST",
        &actions,
        json!({"action":"approve","expected_revision":2}),
    )
    .await;
    assert_eq!(
        ok(&a, "bob", "GET", &path, Value::Null).await["task"]["status"],
        "completed"
    );
    no_execution(&db);

    drop(a);
    let a = app(Hub::open(fixture.config()).unwrap());
    let recovered = ok(&a, "bob", "GET", &receipt_path, Value::Null).await;
    assert_eq!(recovered["response"], receipt["response"]);
    assert_eq!(recovered["current_stale"], true);
    assert_eq!(count(&db, "SELECT count(*) FROM deliveries"), 2);
    assert_eq!(
        request(&a, "alice", "GET", &receipt_path, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    no_execution(&db);
}
