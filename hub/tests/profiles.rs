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
fn profile(revision: u64) -> Value {
    json!({"expected_revision":revision,"agents":[
        {"id":"butler","name":"管家","role":"coordinator","model_ref":"server-default","enabled":true},
        {"id":"research","name":"研究员","role":"researcher","model_ref":"server-default","enabled":true}
    ]})
}
#[tokio::test]
async fn profiles_require_authentication_for_reads_and_writes() {
    let a = setup();
    for (method, path, body) in [
        ("GET", "/api/profile", Value::Null),
        ("GET", "/api/profile/catalog", Value::Null),
        ("PUT", "/api/profile", profile(0)),
    ] {
        assert_eq!(
            request(&a, "mallory", method, path, body).await.0,
            StatusCode::UNAUTHORIZED
        );
    }
}
#[tokio::test]
async fn personal_profiles_are_isolated_persisted_and_revision_checked() {
    let path = std::env::temp_dir().join(format!("agenct-profile-{}.db", uuid::Uuid::new_v4()));
    let a = app(Hub::open(config(path.to_str().unwrap())).unwrap());
    let (status, body) = request(&a, "alice", "PUT", "/api/profile", profile(0)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["revision"], 1);
    assert_eq!(body["agents"].as_array().unwrap().len(), 2);
    let bob = request(&a, "bob", "GET", "/api/profile", Value::Null)
        .await
        .1;
    assert_eq!(bob["revision"], 0);
    assert_eq!(bob["agents"].as_array().unwrap().len(), 1);
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", profile(0))
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&a, "alice", "PUT", "/api/profile", profile(1))
            .await
            .0,
        StatusCode::OK
    );
    drop(a);
    let b = app(Hub::open(config(path.to_str().unwrap())).unwrap());
    assert_eq!(
        request(&b, "alice", "GET", "/api/profile", Value::Null)
            .await
            .1["revision"],
        2
    );
    assert_eq!(
        request(&b, "bob", "GET", "/api/profile", Value::Null)
            .await
            .1["revision"],
        0
    );
    drop(b);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("lock"));
}
#[tokio::test]
async fn profile_rejects_identity_credentials_and_endpoints() {
    let a = setup();
    for key in [
        "owner",
        "user",
        "api_key",
        "endpoint",
        "token",
        "permissions",
    ] {
        let mut body = profile(0);
        body[key] = json!("bob");
        assert_eq!(
            request(&a, "alice", "PUT", "/api/profile", body).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let mut body = profile(0);
        body["agents"][0][key] = json!("forbidden");
        assert_eq!(
            request(&a, "alice", "PUT", "/api/profile", body).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for model in [
        "http://127.0.0.1:9000",
        "https://evil.example",
        "deepseek-key",
        "",
    ] {
        let mut body = profile(0);
        body["agents"][0]["model_ref"] = json!(model);
        assert_eq!(
            request(&a, "alice", "PUT", "/api/profile", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        request(&a, "alice", "GET", "/api/profile", Value::Null)
            .await
            .1["revision"],
        0
    );
}
#[tokio::test]
async fn profile_enforces_bounds_unique_agents_and_single_chat_coordinator() {
    let a = setup();
    let mut invalid = vec![json!({"expected_revision":0,"agents":[]})];
    let mut body = profile(0);
    body["agents"][1]["id"] = json!("butler");
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][0]["enabled"] = json!(false);
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][1]["role"] = json!("coordinator");
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][1]["role"] = json!("admin");
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][0]["name"] = json!("x".repeat(97));
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][0]["name"] = json!("\n");
    invalid.push(body);
    let mut body = profile(0);
    body["agents"][0]["id"] = json!("../bob");
    invalid.push(body);
    let mut body = profile(0);
    body["agents"] = json!(vec![body["agents"][0].clone(); 9]);
    invalid.push(body);
    for body in invalid {
        assert_eq!(
            request(&a, "alice", "PUT", "/api/profile", body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    let catalog = request(&a, "alice", "GET", "/api/profile/catalog", Value::Null)
        .await
        .1;
    assert_eq!(catalog["models"][0]["id"], "server-default");
    assert_eq!(catalog["execution"], "configuration_only");
}

#[tokio::test]
async fn concurrent_profile_updates_cannot_overwrite_each_other() {
    let a = setup();
    let (first, second) = tokio::join!(
        request(&a, "alice", "PUT", "/api/profile", profile(0)),
        request(&a, "alice", "PUT", "/api/profile", profile(0))
    );
    assert!(
        (first.0 == StatusCode::OK && second.0 == StatusCode::CONFLICT)
            || (second.0 == StatusCode::OK && first.0 == StatusCode::CONFLICT)
    );
    assert_eq!(
        request(&a, "alice", "GET", "/api/profile", Value::Null)
            .await
            .1["revision"],
        1
    );
    let response = a
        .oneshot(
            Request::builder()
                .uri("/api/profile")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
