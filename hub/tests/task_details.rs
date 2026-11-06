//! Requester-written shared material attached at confirmation.
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
        let dir = std::env::temp_dir().join(format!("agenct-details-{}", uuid::Uuid::new_v4()));
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
    async fn request(
        &self,
        who: &str,
        method: &str,
        path: &str,
        data: Value,
    ) -> (StatusCode, Value) {
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
    async fn proposal(&self, key: &str) -> Value {
        let (status, p) = self
            .request(
                "alice",
                "POST",
                "/api/proposals",
                json!({"owner":"bob","summary":"审阅 README 快速开始","request_id":key}),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        p
    }
    async fn act(&self, p: &Value, body: Value) -> (StatusCode, Value) {
        let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
        self.request("alice", "POST", &path, body).await
    }
}
const MATERIAL: &str = "## 快速开始\n```sh\npython3 scripts/private-staging.py start\n```";

#[tokio::test]
async fn confirmed_details_are_shared_with_participants_only() {
    let f = Fixture::new();
    let p = f.proposal("p1").await;
    let (status, confirmed) = f
        .act(
            &p,
            json!({"action":"confirm","expected_revision":0,"details":MATERIAL}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let task = confirmed["task_id"].as_str().unwrap().to_string();
    for who in ["alice", "bob"] {
        let (status, detail) = f
            .request(who, "GET", &format!("/api/tasks/{task}"), Value::Null)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["details"]["content"], MATERIAL);
        assert_eq!(detail["details"]["author"], "alice");
        let (_, list) = f.request(who, "GET", "/api/tasks", Value::Null).await;
        assert_eq!(list[0]["details"], MATERIAL);
    }
    assert_eq!(
        f.request("eve", "GET", &format!("/api/tasks/{task}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.request("eve", "GET", "/api/tasks", Value::Null).await.1,
        json!([])
    );
    // Without details, tasks keep an explicit null.
    let q = f.proposal("p2").await;
    f.act(&q, json!({"action":"confirm","expected_revision":0}))
        .await;
    let (_, list) = f.request("bob", "GET", "/api/tasks", Value::Null).await;
    assert!(list
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["details"].is_null()));
}

#[tokio::test]
async fn details_are_bounded_confirm_only_and_replay_exact() {
    let f = Fixture::new();
    let p = f.proposal("p1").await;
    for body in [
        json!({"action":"confirm","expected_revision":0,"details":"   "}),
        json!({"action":"confirm","expected_revision":0,"details":"x".repeat(12001)}),
        json!({"action":"reject","expected_revision":0,"details":"no"}),
        json!({"action":"confirm","expected_revision":0,"details":7}),
    ] {
        assert!(f.act(&p, body).await.0.is_client_error());
    }
    assert_eq!(
        f.request("bob", "GET", "/api/tasks", Value::Null).await.1,
        json!([])
    );
    let ok = json!({"action":"confirm","expected_revision":0,"details":"x".repeat(12000)});
    let (status, first) = f.act(&p, ok.clone()).await;
    assert_eq!(status, StatusCode::OK);
    // Identical replay returns the same task; any different details are refused.
    let (status, again) = f.act(&p, ok).await;
    assert_eq!(
        (status, &again["task_id"]),
        (StatusCode::OK, &first["task_id"])
    );
    for different in [
        json!({"action":"confirm","expected_revision":0,"details":"changed"}),
        json!({"action":"confirm","expected_revision":0}),
    ] {
        assert_eq!(f.act(&p, different).await.0, StatusCode::CONFLICT);
    }
    let db = f.db();
    let rows: i64 = db
        .query_row("SELECT count(*) FROM task_details", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    assert!(db
        .execute("UPDATE task_details SET content='edited'", [])
        .is_err());
    assert!(db.execute("DELETE FROM task_details", []).is_err());
    assert!(db
        .execute("INSERT OR REPLACE INTO task_details(task_id,author,content) SELECT task_id,'eve','x' FROM task_details", [])
        .is_err());
}

#[tokio::test]
async fn details_reach_task_context_for_discussion_and_drafting() {
    let f = Fixture::new();
    let p = f.proposal("p1").await;
    // 12000 stored bytes; newlines JSON-escape to two bytes, so the snapshot excerpt
    // (bounded at 12000 encoded bytes) must truncate and say so.
    let long = "行\n".repeat(3000);
    let (_, confirmed) = f
        .act(
            &p,
            json!({"action":"confirm","expected_revision":0,"details":long}),
        )
        .await;
    let task = confirmed["task_id"].as_str().unwrap().to_string();
    let (status, chat) = f
        .request("alice", "POST", "/api/messages",
            json!({"message":"这份材料够吗？","request_id":"t1","selected_task":{"task_id":task,"expected_revision":0}}))
        .await;
    assert_eq!(status, StatusCode::OK, "{chat}");
    let context = &chat["task_context"];
    assert_eq!(context["details_truncated"], true);
    let excerpt = context["details"].as_str().unwrap();
    assert!(!excerpt.is_empty() && long.starts_with(excerpt) && excerpt.len() <= 12000);
    assert!(context.to_string().len() <= 20480);

    f.request(
        "bob",
        "POST",
        &format!("/api/tasks/{task}/actions"),
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    let (status, draft) = f
        .request(
            "bob",
            "POST",
            &format!("/api/tasks/{task}/drafts"),
            json!({"request_id":"d1","expected_revision":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{draft}");
    assert_eq!(
        draft["response"]["task_context"]["details"],
        context["details"]
    );
}
