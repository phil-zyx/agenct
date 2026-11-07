//! Automatic owner-private drafting when a task enters `working`.
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
    fn new(auto: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("agenct-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: dir.join("hub.db"),
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
            runtime: Runtime::Mock,
        };
        Self {
            app: app(Hub::open(config).unwrap().with_auto_draft(auto)),
            dir,
        }
    }
    fn draft_turns(&self) -> i64 {
        rusqlite::Connection::open(self.dir.join("hub.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM draft_turns", [], |r| r.get(0))
            .unwrap()
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
    async fn task(&self, who: &str, id: &str) -> Value {
        let (_, tasks) = self.call(who, "GET", "/api/tasks", Value::Null).await;
        tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .unwrap()
            .clone()
    }
    async fn delegated(&self) -> String {
        let (_, p) = self
            .call(
                "alice",
                "POST",
                "/api/proposals",
                json!({"owner":"bob","summary":"做个自我介绍","request_id":"p1"}),
            )
            .await;
        let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
        let (status, out) = self
            .call(
                "alice",
                "POST",
                &path,
                json!({"action":"confirm","expected_revision":0}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out["task_id"].as_str().unwrap().to_string()
    }
    async fn act(&self, who: &str, task: &str, body: Value) -> Value {
        let (status, out) = self
            .call(who, "POST", &format!("/api/tasks/{task}/actions"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out
    }
    /// The owner's view of the current version's draft once it settles.
    async fn settled(&self, task: &str) -> Value {
        for _ in 0..200 {
            let draft = self.task("bob", task).await["my_draft"].clone();
            if draft["status"] != "pending" && !draft.is_null() {
                return draft;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("automatic draft did not settle");
    }
}

#[tokio::test]
async fn accept_starts_one_private_draft_per_task_version() {
    let f = Fixture::new(true);
    let task = f.delegated().await;
    assert!(
        f.task("bob", &task).await["my_draft"].is_null(),
        "nothing is drafted before the owner accepts"
    );

    let accepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":0}),
        )
        .await;
    let request_id = accepted["auto_draft"]["request_id"].as_str().unwrap();
    let draft = f.settled(&task).await;
    assert_eq!(draft["status"], "complete");
    assert_eq!(draft["automatic"], true);
    assert_eq!(draft["request_id"], request_id);

    // The owner reads it through the ordinary private draft receipt.
    let (status, receipt) = f
        .call(
            "bob",
            "GET",
            &format!("/api/tasks/{task}/drafts/turns/{request_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert!(receipt["response"]["draft"]["content"].is_string());

    // Never shared, never delivered: the requester sees no draft state, the task
    // stays with the owner, and the requester cannot read the receipt.
    let alice = f.task("alice", &task).await;
    assert!(alice.get("my_draft").is_none());
    assert_eq!(alice["status"], "working");
    assert!(alice["result"].is_null());
    let (status, _) = f
        .call(
            "alice",
            "GET",
            &format!("/api/tasks/{task}/drafts/turns/{request_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(f.draft_turns(), 1, "one task version, one automatic draft");

    // A manual redraft of the same version still works alongside it.
    let (status, _) = f
        .call(
            "bob",
            "POST",
            &format!("/api/tasks/{task}/drafts"),
            json!({"request_id":"manual-1","expected_revision":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.task("bob", &task).await["my_draft"]["automatic"], false);

    f.act(
        "bob",
        &task,
        json!({"action":"deliver","expected_revision":1,"result":"人工审阅后的自我介绍"}),
    )
    .await;
    assert!(
        f.task("bob", &task).await["my_draft"].is_null(),
        "no draft once the task left working"
    );
    // Returned work goes back to the owner, who re-accepts; that version is
    // drafted again automatically.
    let returned = f
        .act(
            "alice",
            &task,
            json!({"action":"return","expected_revision":2}),
        )
        .await;
    assert!(returned.get("auto_draft").is_none());
    let reaccepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":3}),
        )
        .await;
    assert!(reaccepted["auto_draft"]["request_id"]
        .as_str()
        .unwrap()
        .ends_with("-r4"));
    assert_eq!(f.settled(&task).await["automatic"], true);
    assert_eq!(f.draft_turns(), 3);
}

#[tokio::test]
async fn disabled_hub_never_drafts_on_its_own() {
    let f = Fixture::new(false);
    let task = f.delegated().await;
    let accepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":0}),
        )
        .await;
    assert!(accepted.get("auto_draft").is_none());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(f.draft_turns(), 0);
    assert!(f.task("bob", &task).await["my_draft"].is_null());
}
