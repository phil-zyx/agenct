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

async fn setup_queue() -> (Router, String, Value) {
    let hub = Hub::open(config(":memory:")).unwrap();
    let a = app(hub.clone());
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"prepare code"}),
    )
    .await;
    let id = t["id"].as_str().unwrap().to_owned();
    let scope = agenct_hub::worker_api::Scope {
        task_id: id.clone(),
        owner: "bob".into(),
        task_revision: 1,
        repository: "agenct".into(),
        base_commit: "a".repeat(40),
    };
    let a = app(hub.with_worker_scopes(vec![scope]).unwrap());
    let body = json!({"expected_revision":1,"repository":"agenct","base_commit":"a".repeat(40),"request_id":"queue-1","confirm_queue_only":true});
    (a, id, body)
}
async fn accept(a: &Router, id: &str) {
    assert_eq!(
        request(
            a,
            "bob",
            "POST",
            &format!("/api/tasks/{id}/actions"),
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
}
#[tokio::test]
async fn owner_consent_exact_scope_and_accepted_revision_required() {
    let (a, id, body) = setup_queue().await;
    let path = format!("/api/tasks/{id}/worker");
    assert_eq!(
        request(&a, "bob", "POST", &path, body.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    accept(&a, &id).await;
    for who in ["alice", "eve", "mallory"] {
        let expected = if who == "mallory" {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(
            request(&a, who, "POST", &path, body.clone()).await.0,
            expected
        );
        assert_eq!(
            request(&a, who, "GET", &path, Value::Null).await.0,
            expected
        );
    }
    let (_, before) = request(&a, "bob", "GET", &path, Value::Null).await;
    assert_eq!(before["jobs"], json!([]));
    assert_eq!(before["execution_enabled"], false);
    for (key, value, expected) in [
        ("confirm_queue_only", json!(false), StatusCode::BAD_REQUEST),
        ("expected_revision", json!(0), StatusCode::CONFLICT),
        ("repository", json!("other"), StatusCode::CONFLICT),
        ("base_commit", json!("b".repeat(40)), StatusCode::CONFLICT),
        ("executor", json!("shell"), StatusCode::UNPROCESSABLE_ENTITY),
        ("request_id", json!("../bad"), StatusCode::BAD_REQUEST),
    ] {
        let mut bad = body.clone();
        bad[key] = value;
        assert_eq!(
            request(&a, "bob", "POST", &path, bad).await.0,
            expected,
            "{key}"
        );
    }
    let (status, result) = request(&a, "bob", "POST", &path, body.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(result["job"]["status"], "queued");
    assert_eq!(result["job"]["attempt"], 0);
    assert_eq!(result["execution_enabled"], false);
    assert_eq!(result["publication_enabled"], false);
    let (_, replay) = request(&a, "bob", "POST", &path, body.clone()).await;
    assert_eq!(result, replay);
    let mut duplicate = body.clone();
    duplicate["request_id"] = json!("queue-2");
    assert_eq!(
        request(&a, "bob", "POST", &path, duplicate).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&a, "bob", "GET", &path, Value::Null).await.1["jobs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[tokio::test]
async fn cancel_is_owner_scoped_and_replay_never_requeues() {
    let (a, id, body) = setup_queue().await;
    accept(&a, &id).await;
    let path = format!("/api/tasks/{id}/worker");
    let (_, queued) = request(&a, "bob", "POST", &path, body.clone()).await;
    let job = queued["job"]["id"].as_str().unwrap();
    let cancel = format!("{path}/{job}/cancel");
    assert_eq!(
        request(&a, "alice", "POST", &cancel, json!({})).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &a,
            "bob",
            "POST",
            &format!("/api/tasks/wrong/worker/{job}/cancel"),
            json!({})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    for _ in 0..2 {
        assert_eq!(
            request(&a, "bob", "POST", &cancel, json!({})).await.0,
            StatusCode::OK
        );
    }
    let (_, replay) = request(&a, "bob", "POST", &path, body).await;
    assert_eq!(replay["job"]["id"], job);
    assert_eq!(replay["job"]["status"], "cancelled");
    assert_eq!(replay["job"]["attempt"], 0);
}
#[tokio::test]
async fn task_state_changes_atomically_revoke_queue_and_scope() {
    for action in ["cancel", "deliver"] {
        let (a, id, body) = setup_queue().await;
        accept(&a, &id).await;
        let path = format!("/api/tasks/{id}/worker");
        request(&a, "bob", "POST", &path, body.clone()).await;
        let who = if action == "cancel" { "alice" } else { "bob" };
        assert_eq!(
            request(
                &a,
                who,
                "POST",
                &format!("/api/tasks/{id}/actions"),
                json!({"action":action,"expected_revision":1,"result":"manual result"})
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, status) = request(&a, "bob", "GET", &path, Value::Null).await;
        assert_eq!(status["scope"], Value::Null);
        assert_eq!(status["jobs"][0]["status"], "cancelled");
        // Immutable retry returns original cancelled receipt and cannot resurrect it.
        assert_eq!(
            request(&a, "bob", "POST", &path, body.clone()).await.1["job"]["status"],
            "cancelled"
        );
        let mut fresh = body;
        fresh["request_id"] = json!("fresh");
        assert_eq!(
            request(&a, "bob", "POST", &path, fresh).await.0,
            StatusCode::NOT_FOUND
        );
    }
}
#[tokio::test]
async fn admission_disabled_without_operator_scope() {
    let a = app(Hub::open(config(":memory:")).unwrap());
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"work"}),
    )
    .await;
    let id = t["id"].as_str().unwrap();
    accept(&a, id).await;
    let path = format!("/api/tasks/{id}/worker");
    assert_eq!(
        request(&a, "bob", "GET", &path, Value::Null).await.1["scope"],
        Value::Null
    );
    assert_eq!(request(&a,"bob","POST",&path,json!({"expected_revision":1,"repository":"agenct","base_commit":"a".repeat(40),"request_id":"req","confirm_queue_only":true})).await.0,StatusCode::NOT_FOUND);
}
#[test]
fn operator_scope_validation_fails_closed() {
    for (key, value) in [
        ("owner", json!("unknown")),
        ("base_commit", json!("main")),
        ("repository", json!("https://host/repo")),
        ("task_revision", json!(0)),
    ] {
        let mut scope = json!({"task_id":"t","owner":"bob","task_revision":1,"repository":"agenct","base_commit":"a".repeat(40)});
        scope[key] = value;
        assert!(Hub::open(config(":memory:"))
            .unwrap()
            .with_worker_scopes(vec![serde_json::from_value(scope).unwrap()])
            .is_err());
    }
}

#[tokio::test]
async fn queued_receipt_persists_after_restart_without_execution() {
    let path = std::env::temp_dir().join(format!("agenct-queue-{}.db", uuid::Uuid::new_v4()));
    let p = path.to_str().unwrap();
    let hub = Hub::open(config(p)).unwrap();
    let a = app(hub.clone());
    let (_, t) = request(
        &a,
        "alice",
        "POST",
        "/api/tasks",
        json!({"owner":"bob","summary":"work"}),
    )
    .await;
    let id = t["id"].as_str().unwrap();
    accept(&a, id).await;
    let scope = agenct_hub::worker_api::Scope {
        task_id: id.into(),
        owner: "bob".into(),
        task_revision: 1,
        repository: "repo".into(),
        base_commit: "a".repeat(40),
    };
    drop(a);
    let a = app(hub.with_worker_scopes(vec![scope.clone()]).unwrap());
    let endpoint = format!("/api/tasks/{id}/worker");
    let body = json!({"expected_revision":1,"repository":"repo","base_commit":"a".repeat(40),"request_id":"persist","confirm_queue_only":true});
    let (_, receipt) = request(&a, "bob", "POST", &endpoint, body.clone()).await;
    drop(a);
    let a = app(Hub::open(config(p))
        .unwrap()
        .with_worker_scopes(vec![scope])
        .unwrap());
    assert_eq!(request(&a, "bob", "POST", &endpoint, body).await.1, receipt);
    assert_eq!(
        request(&a, "bob", "GET", &endpoint, Value::Null).await.1["jobs"][0]["attempt"],
        0
    );
    drop(a);
    let _ = std::fs::remove_file(path);
}
