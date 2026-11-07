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
        json!({"action":"confirm","expected_revision":0,"details":"x".repeat(262145)}),
        json!({"action":"reject","expected_revision":0,"details":"no"}),
        json!({"action":"confirm","expected_revision":0,"details":7}),
    ] {
        assert!(f.act(&p, body).await.0.is_client_error());
    }
    assert_eq!(
        f.request("bob", "GET", "/api/tasks", Value::Null).await.1,
        json!([])
    );
    let ok = json!({"action":"confirm","expected_revision":0,"details":"x".repeat(262144)});
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
    // exceeds the raw context bound and must be summarized from the full material.
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
    assert!(excerpt.starts_with("[共享材料摘要：已读取全部原文生成") && excerpt.len() <= 12000);
    assert!(excerpt.contains("MOCK"));
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

#[tokio::test]
async fn edited_confirmation_is_atomic_private_and_replay_safe() {
    let f = Fixture::new();
    let p = f.proposal("edited-confirm").await;
    let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
    let body = json!({"action":"confirm","expected_revision":0,"owner":"eve","summary":"手动改写的摘要","details":MATERIAL});
    assert_eq!(
        f.request("bob", "POST", &path, body.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    let (status, confirmed) = f.act(&p, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(confirmed["owner"], "eve");
    assert_eq!(confirmed["summary"], "手动改写的摘要");
    assert_eq!(confirmed["revision"], 1);
    assert_eq!(f.act(&p, body.clone()).await.1, confirmed);
    let mut changed = body.clone();
    changed["summary"] = json!("different");
    assert_eq!(f.act(&p, changed).await.0, StatusCode::CONFLICT);
    let task = confirmed["task_id"].as_str().unwrap();
    assert_eq!(
        f.request("bob", "GET", &format!("/api/tasks/{task}"), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, details) = f
        .request("eve", "GET", &format!("/api/tasks/{task}"), Value::Null)
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(details["details"]["content"], MATERIAL);
    assert_eq!(
        f.db()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn invalid_edits_do_not_consume_pending_proposal() {
    let f = Fixture::new();
    let p = f.proposal("invalid-edit").await;
    for body in [
        json!({"action":"confirm","expected_revision":0,"owner":"unknown"}),
        json!({"action":"confirm","expected_revision":0,"owner":"alice"}),
        json!({"action":"confirm","expected_revision":0,"summary":"  "}),
        json!({"action":"confirm","expected_revision":0,"summary":"x".repeat(4001)}),
        json!({"action":"reject","expected_revision":0,"owner":"eve"}),
        json!({"action":"confirm","expected_revision":9,"summary":"edited"}),
    ] {
        assert!(f.act(&p, body).await.0.is_client_error());
        assert_eq!(
            f.db()
                .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        f.act(&p, json!({"action":"confirm","expected_revision":0}))
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn pdf_extraction_rejects_invalid_and_unconfigured_callers_without_persistence() {
    let f = Fixture::new();
    assert_eq!(
        f.request(
            "unknown",
            "POST",
            "/api/materials/extract",
            json!({"pdf_base64":"JVBERi0="})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    for body in [
        json!({"pdf_base64":"not base64"}),
        json!({"pdf_base64":"aGVsbG8="}),
        json!({"pdf_base64":"JVBERi0=","owner":"bob"}),
    ] {
        assert!(f
            .request("alice", "POST", "/api/materials/extract", body)
            .await
            .0
            .is_client_error());
    }
    assert_eq!(
        f.db()
            .query_row("SELECT COUNT(*) FROM task_details", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        f.db()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn adopted_draft_provenance_is_validated_and_private_after_manual_edit() {
    let f = Fixture::new();
    let p = f.proposal("adoption").await;
    let (_, p) = f
        .act(&p, json!({"action":"confirm","expected_revision":0}))
        .await;
    let task = p["task_id"].as_str().unwrap();
    let path = format!("/api/tasks/{task}/actions");
    assert_eq!(
        f.request(
            "bob",
            "POST",
            &path,
            json!({"action":"accept","expected_revision":0,"source_draft_id":"forged"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        f.request(
            "bob",
            "POST",
            &path,
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, draft) = f
        .request(
            "bob",
            "POST",
            &format!("/api/tasks/{task}/drafts"),
            json!({"request_id":"draft-adoption","expected_revision":1}),
        )
        .await;
    let source = draft["response"]["draft"]["id"].as_str().unwrap();
    assert_eq!(f.request("bob", "POST", &path, json!({"action":"deliver","expected_revision":1,"result":"Edited by human","source_draft_id":"forged"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(f.request("bob", "POST", &path, json!({"action":"deliver","expected_revision":1,"result":"Edited by human","source_draft_id":source})).await.0, StatusCode::OK);
    assert_eq!(
        f.db()
            .query_row(
                "SELECT source_draft_id FROM deliveries WHERE task_id=?1 AND status='submitted'",
                [task],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        source
    );
    let (_, public) = f
        .request("alice", "GET", &format!("/api/tasks/{task}"), Value::Null)
        .await;
    assert!(public.to_string().contains("Edited by human"));
    assert!(
        !public.to_string().contains(source),
        "private source draft IDs must not be included in public timeline"
    );
}
