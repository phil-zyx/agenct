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
#[tokio::test]
async fn unauthorized_is_rejected() {
    let a = setup();
    for path in ["/api/me", "/api/tasks", "/api/proposals"] {
        assert_eq!(
            request(&a, "mallory", "GET", path, Value::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        request(
            &a,
            "mallory",
            "POST",
            "/api/proposals",
            json!({"owner":"bob","summary":"unauthorized"})
        )
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
async fn direct_task_creation_is_removed_without_storage_side_effects() {
    let dir = std::env::temp_dir().join(format!("agenct-no-direct-task-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("hub.db");
    let a = app(Hub::open(config(path.to_str().unwrap())).unwrap());
    let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY;
    let db = rusqlite::Connection::open_with_flags(&path, flags).unwrap();
    for _ in 0..2 {
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/tasks",
                json!({"owner":"bob","summary":"must not be shared directly"})
            )
            .await
            .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
    for table in ["tasks", "events", "proposals"] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "legacy POST wrote to {table}");
    }
    for who in ["alice", "bob", "eve"] {
        assert_eq!(
            request(&a, who, "GET", "/api/tasks", Value::Null).await,
            (StatusCode::OK, json!([]))
        );
    }
    assert_eq!(
        request(&a, "mallory", "GET", "/api/tasks", Value::Null)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let task = confirmed_task(&a, "alice", "bob", "explicitly confirmed summary").await;
    assert_eq!(task["requester"], "alice");
    assert_eq!(task["owner"], "bob");
    assert_eq!(task["summary"], "explicitly confirmed summary");
    assert_eq!(task["status"], "awaiting_owner");
    assert_eq!(task["revision"], 0);
    for table in ["tasks", "events"] {
        let count: i64 = db
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "confirmation must create exactly one {table} row");
    }
    drop(db);
    drop(a);
    std::fs::remove_dir_all(dir).unwrap();
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
    let t = confirmed_task(&a, "alice", "bob", "shared task only").await;
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
    let t = confirmed_task(&a, "alice", "bob", "x").await;
    let p = format!("/api/tasks/{}/actions", t["id"].as_str().unwrap());
    request(
        &a,
        "alice",
        "POST",
        &p,
        json!({"action":"cancel","expected_revision":0,"reason":"This task is no longer needed"}),
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
    let t = confirmed_task(&a, "alice", "bob", "x").await;
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
        axum::Json(json!({"reply":"adapter contract response","backend":"dsh","proposal":null}))
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
        ("alice", "ask bob to send a task"),
    ] {
        let (status, value) =
            request(&a, who, "POST", "/api/messages", json!({"message":message})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["backend"], "dsh");
    }
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
    server.abort();
}

#[tokio::test]
async fn stale_actions_cannot_accept_unseen_redelivery() {
    let a = setup();
    let t = confirmed_task(&a, "alice", "bob", "review").await;
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

#[tokio::test]
async fn natural_language_proposal_requires_confirmation_and_is_private() {
    let app = setup();
    request(
        &app,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":"private secret 9321"}),
    )
    .await;
    let input = json!({"message":"请让 bob 帮我 检查公开文档","request_id":"natural-1"});
    let (code, reply) = request(&app, "alice", "POST", "/api/messages", input.clone()).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(reply["backend"], "mock");
    let p = &reply["proposal"];
    assert_eq!(p["summary"], "检查公开文档");
    assert_eq!(p["status"], "pending");
    assert_eq!(
        request(&app, "alice", "POST", "/api/messages", input)
            .await
            .1["proposal"]["id"],
        p["id"]
    );
    for who in ["alice", "bob", "eve"] {
        assert_eq!(
            request(&app, who, "GET", "/api/tasks", Value::Null).await.1,
            json!([])
        );
    }
    assert_eq!(
        request(&app, "bob", "GET", "/api/proposals", Value::Null)
            .await
            .1,
        json!([])
    );
    let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
    for who in ["bob", "eve", "mallory"] {
        assert_eq!(
            request(
                &app,
                who,
                "POST",
                &path,
                json!({"action":"confirm","expected_revision":0})
            )
            .await
            .0,
            if who == "mallory" {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::NOT_FOUND
            }
        );
    }
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &path,
            json!({"action":"confirm","expected_revision":5})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let confirm = json!({"action":"confirm","expected_revision":0});
    let (a, b) = tokio::join!(
        request(&app, "alice", "POST", &path, confirm.clone()),
        request(&app, "alice", "POST", &path, confirm)
    );
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(a, b);
    let task = a.1["task_id"].as_str().unwrap();
    let inbox = request(&app, "bob", "GET", "/api/tasks", Value::Null)
        .await
        .1;
    assert_eq!(inbox.as_array().unwrap().len(), 1);
    assert!(!inbox.to_string().contains("9321"));
    assert_eq!(
        request(&app, "bob", "GET", "/api/messages", Value::Null)
            .await
            .1,
        json!([])
    );
    let action = format!("/api/tasks/{task}/actions");
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &action,
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .1["status"],
        "working"
    );
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &action,
            json!({"action":"deliver","expected_revision":1,"result":"文档检查完成"})
        )
        .await
        .1["status"],
        "ready_for_acceptance"
    );
    let detail = request(
        &app,
        "alice",
        "GET",
        &format!("/api/tasks/{task}"),
        Value::Null,
    )
    .await
    .1;
    assert_eq!(detail["task"]["result"], "文档检查完成");
    assert_eq!(detail["events"].as_array().unwrap().len(), 3);
    assert_eq!(
        request(
            &app,
            "eve",
            "GET",
            &format!("/api/tasks/{task}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &action,
            json!({"action":"approve","expected_revision":2})
        )
        .await
        .1["status"],
        "completed"
    );
}
#[tokio::test]
async fn proposals_reject_forgery_reuse_and_rejected_confirmation() {
    let app = setup();
    for input in [
        json!({"owner":"mallory","summary":"x"}),
        json!({"owner":"alice","summary":"x"}),
        json!({"owner":"bob","summary":" "}),
        json!({"owner":"bob","summary":"x","request_id":"a b"}),
    ] {
        assert_eq!(
            request(&app, "alice", "POST", "/api/proposals", input)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for field in ["requester", "status", "task_id", "revision"] {
        let mut input = json!({"owner":"bob","summary":"x"});
        input[field] = json!("forged");
        assert_eq!(
            request(&app, "alice", "POST", "/api/proposals", input)
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    let p = request(
        &app,
        "alice",
        "POST",
        "/api/proposals",
        json!({"owner":"bob","summary":"review","request_id":"stable"}),
    )
    .await
    .1;
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            "/api/proposals",
            json!({"owner":"bob","summary":"different","request_id":"stable"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    // Keys are scoped to authenticated sender, not globally shared.
    assert_eq!(
        request(
            &app,
            "eve",
            "POST",
            "/api/proposals",
            json!({"owner":"bob","summary":"different","request_id":"stable"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &path,
            json!({"action":"confirm","expected_revision":0,"requester":"eve"})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &app,
                "alice",
                "POST",
                &path,
                json!({"action":"reject","expected_revision":0})
            )
            .await
            .1["status"],
            "rejected"
        );
    }
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &path,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&app, "bob", "GET", "/api/tasks", Value::Null)
            .await
            .1,
        json!([])
    );
}
#[tokio::test]
async fn proposal_retries_survive_restart_without_duplicate_tasks() {
    let dir = std::env::temp_dir().join(format!("agenct-proposal-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("hub.db");
    let p = path.to_str().unwrap();
    let a = app(Hub::open(config(p)).unwrap());
    let input = json!({"owner":"bob","summary":"restart test","request_id":"persisted"});
    let proposal = request(&a, "alice", "POST", "/api/proposals", input.clone())
        .await
        .1;
    let action = format!(
        "/api/proposals/{}/actions",
        proposal["id"].as_str().unwrap()
    );
    let confirmed = request(
        &a,
        "alice",
        "POST",
        &action,
        json!({"action":"confirm","expected_revision":0}),
    )
    .await
    .1;
    drop(a);
    let a = app(Hub::open(config(p)).unwrap());
    assert_eq!(
        request(&a, "alice", "POST", "/api/proposals", input)
            .await
            .1,
        confirmed
    );
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &action,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .1,
        confirmed
    );
    assert_eq!(
        request(&a, "bob", "GET", "/api/tasks", Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );
    drop(a);
    std::fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn removed_recipient_blocks_confirmation_but_not_rejection() {
    let dir = std::env::temp_dir().join(format!("agenct-removed-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("hub.db");
    let p = path.to_str().unwrap();
    let a = app(Hub::open(config(p)).unwrap());
    let proposal = request(
        &a,
        "alice",
        "POST",
        "/api/proposals",
        json!({"owner":"bob","summary":"review"}),
    )
    .await
    .1;
    let action = format!(
        "/api/proposals/{}/actions",
        proposal["id"].as_str().unwrap()
    );
    drop(a);
    let mut changed = config(p);
    changed.users.retain(|u| u.id != "bob");
    let a = app(Hub::open(changed).unwrap());
    assert_eq!(
        request(
            &a,
            "alice",
            "POST",
            &action,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&a, "alice", "GET", "/api/tasks", Value::Null)
            .await
            .1,
        json!([])
    );
    for _ in 0..2 {
        let (status, rejected) = request(
            &a,
            "alice",
            "POST",
            &action,
            json!({"action":"reject","expected_revision":0}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rejected["status"], "rejected");
        assert_eq!(rejected["revision"], 1);
        assert!(rejected["task_id"].is_null());
    }
    drop(a);
    std::fs::remove_dir_all(dir).unwrap();
}

async fn dsh_app(response: Value) -> (Router, tokio::task::JoinHandle<()>) {
    let adapter = Router::new().route(
        "/v1/turn",
        axum::routing::post(move || {
            let value = response.clone();
            async move { axum::Json(value) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut cfg = config(":memory:");
    cfg.runtime = Runtime::Dsh {
        url: format!("http://{}/v1/turn", listener.local_addr().unwrap()),
        token: "test-internal-token".into(),
    };
    let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
    (app(Hub::open(cfg).unwrap()), server)
}

#[tokio::test]
async fn dsh_proposal_stays_private_until_explicit_confirmation() {
    let (app, server) = dsh_app(json!({"backend":"dsh","reply":"Review draft", "proposal":{"owner":"bob","summary":"review tests"}})).await;
    let (status, turn) = request(
        &app,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":"Ask bob to review tests","request_id":"dsh-1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(turn["backend"], "dsh");
    assert_eq!(turn["proposal"]["status"], "pending");
    for who in ["bob", "eve"] {
        assert_eq!(
            request(&app, who, "GET", "/api/tasks", Value::Null).await.1,
            json!([])
        );
        assert_eq!(
            request(&app, who, "GET", "/api/proposals", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            request(&app, who, "GET", "/api/messages", Value::Null)
                .await
                .1,
            json!([])
        );
    }
    let path = format!(
        "/api/proposals/{}/actions",
        turn["proposal"]["id"].as_str().unwrap()
    );
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &path,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for _ in 0..2 {
        assert_eq!(
            request(
                &app,
                "alice",
                "POST",
                &path,
                json!({"action":"confirm","expected_revision":0})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let tasks = request(&app, "bob", "GET", "/api/tasks", Value::Null)
        .await
        .1;
    assert_eq!(tasks.as_array().unwrap().len(), 1);
    assert_eq!(tasks[0]["summary"], "review tests");
    server.abort();
}

#[tokio::test]
async fn dsh_invalid_proposals_fail_closed_without_tasks_or_fallback() {
    let base = json!({"backend":"dsh","reply":"Draft","proposal":null});
    let bad = vec![
        json!({"owner":"bob","summary":"private history"}),
        json!({"owner":"mallory","summary":"review tests"}),
        json!({"owner":"alice","summary":"review tests"}),
        json!({"owner":"eve","summary":"review tests"}),
        json!({"owner":"bob","summary":"review tests","confirm":true}),
        json!({"owner":"bob","summary":" "}),
        json!({"owner":"bob","summary":"review tests","action":"send"}),
    ];
    for proposal in bad {
        let mut response = base.clone();
        response["proposal"] = proposal;
        let (app, server) = dsh_app(response).await;
        assert_eq!(
            request(
                &app,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"Ask bob to review tests"})
            )
            .await
            .0,
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            request(&app, "alice", "GET", "/api/proposals", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            request(&app, "bob", "GET", "/api/tasks", Value::Null)
                .await
                .1,
            json!([])
        );
        let history = request(&app, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1;
        assert_eq!(history[0]["status"], "failed");
        assert_eq!(history.as_array().unwrap().len(), 1);
        server.abort();
    }
}

#[tokio::test]
async fn dsh_requires_explicit_nullable_proposal_field() {
    for (response, expected) in [
        (
            json!({"backend":"dsh","reply":"Hello","proposal":null}),
            StatusCode::OK,
        ),
        (
            json!({"backend":"dsh","reply":"Hello"}),
            StatusCode::BAD_GATEWAY,
        ),
        (
            json!({"backend":"dsh","reply":"Hello","proposal":null,"execute":true}),
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        let (app, server) = dsh_app(response).await;
        assert_eq!(
            request(
                &app,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"Hello"})
            )
            .await
            .0,
            expected
        );
        assert_eq!(
            request(&app, "alice", "GET", "/api/proposals", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            request(&app, "alice", "GET", "/api/tasks", Value::Null)
                .await
                .1,
            json!([])
        );
        server.abort();
    }
}

#[tokio::test]
async fn mock_chinese_summary_accepts_optional_whitespace_without_task_authority() {
    let app = setup();
    for (index, separator) in ["", " ", "  ", "\t", "\u{3000}"].iter().enumerate() {
        let (status, response) = request(
            &app,
            "alice",
            "POST",
            "/api/messages",
            json!({
                "message":format!("请让 bob 帮我{separator}整理公开文档"),
                "request_id":format!("spacing-{index}")
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["backend"], "mock");
        assert_eq!(response["proposal"]["owner"], "bob");
        assert_eq!(response["proposal"]["summary"], "整理公开文档");
        assert_eq!(response["proposal"]["status"], "pending");
    }
    for message in [
        "请让 bob 帮我",
        "请让 bob 帮我   ",
        "请让 bob 帮我\u{3000}",
        "请让 mallory 帮我整理文档",
        "请让 alice 帮我整理文档",
    ] {
        assert_eq!(
            request(
                &app,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":message})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    // English still requires the explicit 'to ' boundary, not a prefix such as 'today'.
    let (_, response) = request(
        &app,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":"ask bob today"}),
    )
    .await;
    assert!(response["proposal"].is_null());
    for user in ["alice", "bob"] {
        assert_eq!(
            request(&app, user, "GET", "/api/tasks", Value::Null)
                .await
                .1,
            json!([])
        );
    }
    assert_eq!(
        request(&app, "bob", "GET", "/api/proposals", Value::Null)
            .await
            .1,
        json!([])
    );
}

#[tokio::test]
async fn invalid_mock_runtime_proposals_never_persist_history_tasks_or_proposals() {
    let app = setup();
    for (index, message) in ["ask alice to review", "ask mallory to review"]
        .iter()
        .enumerate()
    {
        let body = json!({"message":message,"request_id":format!("invalid-mock-{index}")});
        assert_eq!(
            request(&app, "alice", "POST", "/api/messages", body.clone())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        // The reserved failed request is not dispatched again, including by retries.
        assert_eq!(
            request(&app, "alice", "POST", "/api/messages", body)
                .await
                .1["turn"]["status"],
            "uncertain"
        );
    }
    for who in ["alice", "bob", "eve"] {
        for endpoint in ["/api/messages", "/api/tasks", "/api/proposals"] {
            assert_eq!(
                request(&app, who, "GET", endpoint, Value::Null).await.1,
                json!([])
            );
        }
    }
}

#[tokio::test]
async fn deliveries_preserve_three_versions_after_two_returns() {
    let a = setup();
    let task = confirmed_task(&a, "alice", "bob", "immutable evidence").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let actions = format!("{path}/actions");
    let mut revision = task["revision"].as_i64().unwrap();
    for version in 1..=3 {
        assert_eq!(
            request(
                &a,
                "bob",
                "POST",
                &actions,
                json!({"action":"accept","expected_revision":revision})
            )
            .await
            .0,
            StatusCode::OK
        );
        revision += 1;
        let deliver = json!({"action":"deliver","expected_revision":revision,"result":format!("version {version}")});
        assert_eq!(
            request(&a, "alice", "POST", &actions, deliver.clone())
                .await
                .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            request(&a, "bob", "POST", &actions, deliver.clone())
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            request(&a, "bob", "POST", &actions, deliver).await.0,
            StatusCode::CONFLICT
        );
        revision += 1;
        if version < 3 {
            assert_eq!(
                request(
                    &a,
                    "alice",
                    "POST",
                    &actions,
                    json!({"action":"return","expected_revision":revision})
                )
                .await
                .0,
                StatusCode::OK
            );
            revision += 1;
            let (_, detail) = request(&a, "alice", "GET", &path, Value::Null).await;
            assert!(detail["task"]["result"].is_null());
            assert_eq!(detail["deliveries"].as_array().unwrap().len(), version);
        }
    }
    for who in ["alice", "bob"] {
        let (_, detail) = request(&a, who, "GET", &path, Value::Null).await;
        let rows = detail["deliveries"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row["seq"], i + 1);
            assert_eq!(row["content"], format!("version {}", i + 1));
            assert_eq!(row["author"], "bob");
            assert_eq!(row.as_object().unwrap().len(), 5);
        }
    }
    assert_eq!(
        request(&a, "eve", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn private_drafts_do_not_leak_or_consume_submitted_sequence() {
    let directory =
        std::env::temp_dir().join(format!("agenct-deliveries-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("hub.db");
    let a = app(Hub::open(config(path.to_str().unwrap())).unwrap());
    let task = confirmed_task(&a, "alice", "bob", "private drafting").await;
    let id = task["id"].as_str().unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute("INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision) VALUES ('secret-draft',?1,99,'bob','secret content','draft',0)",[id]).unwrap();
    let endpoint = format!("/api/tasks/{id}");
    let actions = format!("{endpoint}/actions");
    let revision = task["revision"].as_i64().unwrap();
    request(
        &a,
        "bob",
        "POST",
        &actions,
        json!({"action":"accept","expected_revision":revision}),
    )
    .await;
    db.execute_batch("CREATE TRIGGER fail_delivery_event BEFORE INSERT ON events WHEN NEW.action='deliver' BEGIN SELECT RAISE(ABORT,'injected event failure'); END").unwrap();
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &actions,
            json!({"action":"deliver","expected_revision":revision+1,"result":"rolled back"})
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM deliveries WHERE status='submitted'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let (_, before) = request(&a, "alice", "GET", &endpoint, Value::Null).await;
    assert_eq!(before["task"]["revision"], revision + 1);
    assert_eq!(before["task"]["status"], "working");
    db.execute_batch("DROP TRIGGER fail_delivery_event")
        .unwrap();
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &actions,
            json!({"action":"deliver","expected_revision":revision+1,"result":"public result"})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (_, detail) = request(&a, "alice", "GET", &endpoint, Value::Null).await;
    assert_eq!(detail["deliveries"].as_array().unwrap().len(), 1);
    assert_eq!(detail["deliveries"][0]["seq"], 1);
    assert!(!detail.to_string().contains("secret"));
    assert_eq!(
        db.query_row(
            "SELECT content FROM deliveries WHERE id='secret-draft'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "secret content"
    );
    assert!(db
        .execute("UPDATE deliveries SET content='changed'", [])
        .is_err());
    assert!(db.execute("DELETE FROM deliveries", []).is_err());
    drop(db);
    drop(a);
    std::fs::remove_dir_all(directory).unwrap();
}
