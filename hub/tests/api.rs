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
fn setup() -> Router {
    app(Hub::open(config(":memory:")).unwrap())
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
#[tokio::test]
async fn unauthorized_is_rejected() {
    assert_eq!(
        request(&setup(), "mallory", "GET", "/api/me", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn messages_are_scoped_and_persist() {
    let path = std::env::temp_dir().join(format!("agenct-{}.db", uuid::Uuid::new_v4()));
    let p = path.to_str().unwrap();
    let a = app(Hub::open(config(p)).unwrap());
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"alice secret"})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        request(&a, "bob", "GET", "/api/messages", Value::Null)
            .await
            .1,
        json!([])
    );
    drop(a);
    let b = app(Hub::open(config(p)).unwrap());
    assert_eq!(
        request(&b, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1[0]["content"],
        "alice secret"
    );
    drop(b);
    let _ = std::fs::remove_file(path);
}
#[tokio::test]
async fn caller_cannot_choose_identity_or_session() {
    let a = setup();
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"x","owner":"bob"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
#[tokio::test]
async fn task_lifecycle_checks_roles_and_no_private_history() {
    let a = setup();
    request(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":"private note"}),
    )
    .await;
    let (s, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"shared task only"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let path = format!("/api/tasks/{}/actions", t["id"].as_str().unwrap());
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &path,
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&a, "eve", "GET", &path.replace("/actions", ""), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(&a, "bob", "GET", "/api/messages", Value::Null)
            .await
            .1,
        json!([])
    );
    for (revision, (u, act, extra)) in [
        ("bob", "accept", Value::Null),
        ("bob", "deliver", json!("human-authored result")),
        ("alice", "approve", Value::Null),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            request(
                &a,
                u,
                "POST",
                &path,
                json!({"action":act,"result":extra,"expected_revision":revision})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &path,
            json!({"action":"approve","expected_revision":3})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&a, "alice", "GET", "/api/tasks", Value::Null)
            .await
            .1[0]["status"],
        "completed"
    );
    assert_eq!(
        request(&a, "eve", "GET", "/api/tasks", Value::Null).await.1,
        json!([])
    );
}
#[tokio::test]
async fn cancellation_is_terminal() {
    let a = setup();
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"x"}),
    )
    .await;
    let p = format!("/api/tasks/{}/actions", t["id"].as_str().unwrap());
    request(
        &a,
        "alice",
        "POST",
        &p,
        json!({"action":"cancel","expected_revision":0}),
    )
    .await;
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &p,
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
}
#[tokio::test]
async fn adapter_failure_never_silently_falls_back() {
    let mut c = config(":memory:");
    c.runtime = Runtime::Dsh {
        url: "http://127.0.0.1:1/v1/turn".into(),
        token: token("adapter"),
    };
    let a = app(Hub::open(c).unwrap());
    assert_eq!(
        request(&a, "alice", "POST", "/api/messages", json!({"message":"x"}))
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
    let (_, m) = request(&a, "alice", "GET", "/api/messages", Value::Null).await;
    assert_eq!(m.as_array().unwrap().len(), 1);
    assert_eq!(m[0]["status"], "failed");
}
#[tokio::test]
async fn result_required_before_delivery() {
    let a = setup();
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"x"}),
    )
    .await;
    let p = format!("/api/tasks/{}/actions", t["id"].as_str().unwrap());
    request(
        &a,
        "bob",
        "POST",
        &p,
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &p,
            json!({"action":"deliver","result":" ","expected_revision":1})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn adapter_contract_uses_scoped_role_content_history() {
    async fn endpoint(axum::Json(v): axum::Json<Value>) -> axum::Json<Value> {
        assert!(uuid::Uuid::parse_str(v["session_id"].as_str().unwrap()).is_ok());
        for m in v["history"].as_array().unwrap() {
            assert_eq!(m.as_object().unwrap().len(), 2);
            assert!(m.get("role").is_some() && m.get("content").is_some());
            assert_ne!(m["content"], "bob private");
        }
        axum::Json(json!({"reply":"adapter contract response","backend":"dsh"}))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/v1/turn", axum::routing::post(endpoint)),
        )
        .await
        .unwrap();
    });
    let mut c = config(":memory:");
    c.runtime = Runtime::Dsh {
        url: format!("http://{address}/v1/turn"),
        token: "t".repeat(40),
    };
    let a = app(Hub::open(c).unwrap());
    for (who, message) in [
        ("bob", "bob private"),
        ("alice", "hello"),
        ("alice", "followup"),
    ] {
        let (status, value) =
            request(&a, who, "POST", "/api/messages", json!({"message":message})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["backend"], "dsh");
    }
    server.abort();
}

#[tokio::test]
async fn stale_actions_cannot_accept_unseen_redelivery() {
    let a = setup();
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"review"}),
    )
    .await;
    let path = format!("/api/tasks/{}/actions", t["id"].as_str().unwrap());
    for (revision, (who, action, result)) in [
        ("bob", "accept", Value::Null),
        ("bob", "deliver", json!("original")),
        ("alice", "return", Value::Null),
        ("bob", "accept", Value::Null),
        ("bob", "deliver", json!("different result")),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            request(
                &a,
                who,
                "POST",
                &path,
                json!({"action":action,"result":result,"expected_revision":revision})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for action in ["approve", "return", "cancel"] {
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                &path,
                json!({"action":action,"expected_revision":2})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
    }
    assert_eq!(
        request(&a, "alice", "POST", &path, json!({"action":"approve"}))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (_, detail) = request(
        &a,
        "alice",
        "GET",
        &path.replace("/actions", ""),
        Value::Null,
    )
    .await;
    assert_eq!(detail["task"]["result"], "different result");
    assert_eq!(detail["task"]["revision"], 5);
    assert_eq!(detail["events"].as_array().unwrap().len(), 6);
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &path,
            json!({"action":"approve","expected_revision":5})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn existing_task_schema_is_migrated() {
    let path = std::env::temp_dir().join(format!("agenct-migrate-{}.db", uuid::Uuid::new_v4()));
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE tasks (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL, summary TEXT NOT NULL, status TEXT NOT NULL, result TEXT); INSERT INTO tasks VALUES ('old','alice','bob','old task','awaiting_owner',NULL);").unwrap();
    drop(conn);
    let a = app(Hub::open(config(path.to_str().unwrap())).unwrap());
    let (_, tasks) = request(&a, "alice", "GET", "/api/tasks", Value::Null).await;
    assert_eq!(tasks[0]["revision"], 0);
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            "/api/tasks/old/actions",
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    drop(a);
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(path.with_extension("lock")).unwrap();
}
