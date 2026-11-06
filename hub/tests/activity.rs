//! Per-user work index: latest step per unit of work, under existing visibility rules.
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
struct Fixture {
    app: Router,
    dir: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("agenct-activity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: dir.join("hub.db"),
            users: ["alice", "bob", "eve"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
            runtime: Runtime::Mock,
        };
        Self {
            app: app(Hub::open(config).unwrap()),
            dir,
        }
    }
    fn db(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.dir.join("hub.db")).unwrap()
    }
    async fn call(&self, who: &str, method: &str, path: &str, data: Value) -> (StatusCode, Value) {
        let r = self
            .app
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
    async fn activity(&self, who: &str) -> Vec<Value> {
        let (status, items) = self.call(who, "GET", "/api/activity", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        items.as_array().unwrap().clone()
    }
    async fn propose(&self, key: &str, summary: &str) -> Value {
        let (status, p) = self
            .call(
                "alice",
                "POST",
                "/api/proposals",
                json!({"owner":"bob","summary":summary,"request_id":key}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        p
    }
    async fn proposal_action(&self, p: &Value, action: &str) -> Value {
        let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
        let (status, out) = self
            .call(
                "alice",
                "POST",
                &path,
                json!({"action":action,"expected_revision":0}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        out
    }
    async fn task_action(&self, who: &str, task: &str, body: Value) {
        let (status, out) = self
            .call(who, "POST", &format!("/api/tasks/{task}/actions"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
    }
}
fn latest(items: &[Value], id: &str) -> Value {
    items
        .iter()
        .find(|i| i["id"] == id)
        .map(|i| i["latest"].clone())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn index_follows_lifecycle_and_keeps_drafts_private() {
    let f = Fixture::new();
    assert!(f.activity("alice").await.is_empty());

    let p = f.propose("p1", "审阅 README").await;
    assert!(p["created_at"].is_string());
    let alice = f.activity("alice").await;
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0]["kind"], "proposal");
    assert_eq!(alice[0]["counterpart"], "bob");
    assert_eq!(alice[0]["latest"]["action"], "proposal_pending");
    assert!(alice[0]["latest"]["at"].is_string());
    // Proposals are sender-private.
    assert!(f.activity("bob").await.is_empty());

    let task = f.proposal_action(&p, "confirm").await["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    let alice = f.activity("alice").await;
    assert_eq!(
        alice.len(),
        1,
        "a confirmed proposal is represented by its task"
    );
    assert_eq!(
        (alice[0]["kind"].as_str(), alice[0]["role"].as_str()),
        (Some("task"), Some("requester"))
    );
    let bob = f.activity("bob").await;
    assert_eq!(
        (bob[0]["role"].as_str(), bob[0]["counterpart"].as_str()),
        (Some("owner"), Some("alice"))
    );
    assert_eq!(latest(&bob, &task)["action"], "created");

    f.task_action(
        "bob",
        &task,
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    assert_eq!(
        latest(&f.activity("alice").await, &task)["action"],
        "accept"
    );

    let (status, _) = f
        .call(
            "bob",
            "POST",
            &format!("/api/tasks/{task}/drafts"),
            json!({"request_id":"d1","expected_revision":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // The owner sees their private draft step; the requester never does.
    assert_eq!(
        latest(&f.activity("bob").await, &task)["action"],
        "draft_complete"
    );
    let alice = f.activity("alice").await;
    assert_eq!(latest(&alice, &task)["action"], "accept");
    assert!(!serde_json::to_string(&alice).unwrap().contains("draft"));

    f.task_action(
        "bob",
        &task,
        json!({"action":"deliver","expected_revision":1,"result":"done"}),
    )
    .await;
    // A later shared transition supersedes the draft even within the same second.
    assert_eq!(latest(&f.activity("bob").await, &task)["action"], "deliver");
    assert_eq!(
        latest(&f.activity("alice").await, &task)["action"],
        "deliver"
    );
    assert!(f.activity("eve").await.is_empty());
}

#[tokio::test]
async fn cancel_reason_and_rejected_proposals_are_indexed() {
    let f = Fixture::new();
    let p = f.propose("p1", "整理会议纪要").await;
    let task = f.proposal_action(&p, "confirm").await["task_id"]
        .as_str()
        .unwrap()
        .to_string();
    f.task_action(
        "alice",
        &task,
        json!({"action":"cancel","expected_revision":0,"reason":"不需要了"}),
    )
    .await;
    for who in ["alice", "bob"] {
        let l = latest(&f.activity(who).await, &task);
        assert_eq!(
            (
                l["action"].as_str(),
                l["reason"].as_str(),
                l["actor"].as_str()
            ),
            (Some("cancel"), Some("不需要了"), Some("alice"))
        );
    }
    let r = f.propose("p2", "写周报").await;
    f.proposal_action(&r, "reject").await;
    let alice = f.activity("alice").await;
    assert_eq!(
        latest(&alice, r["id"].as_str().unwrap())["action"],
        "proposal_rejected"
    );
    assert!(f.activity("bob").await.iter().all(|i| i["kind"] == "task"));
}

#[tokio::test]
async fn timestamps_on_new_records_and_undated_legacy_rows_sort_last() {
    let f = Fixture::new();
    let (status, chat) = f
        .call(
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"你好","request_id":"m1"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{chat}");
    let (_, messages) = f.call("alice", "GET", "/api/messages", Value::Null).await;
    assert!(messages
        .as_array()
        .unwrap()
        .iter()
        .all(|m| m["created_at"].is_string()));

    f.propose("new", "新提案").await;
    f.db()
        .execute(
            "INSERT INTO proposals(id,requester,owner,summary,request_id,source) VALUES ('legacy','alice','bob','旧提案','old','旧提案')",
            [],
        )
        .unwrap();
    let alice = f.activity("alice").await;
    assert_eq!(alice.len(), 2);
    assert_eq!(alice[0]["title"], "新提案");
    assert_eq!(alice[1]["id"], "legacy");
    assert!(alice[1]["latest"]["at"].is_null());
}
