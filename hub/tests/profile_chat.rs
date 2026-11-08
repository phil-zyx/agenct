//! Conversational profile changes remain private suggestions until explicitly confirmed.
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

fn setup() -> Router {
    app(Hub::open(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: ":memory:".into(),
        runtime: Runtime::Mock,
        users: ["alice", "bob"]
            .map(|id| UserConfig {
                id: id.into(),
                token_sha256: format!(
                    "{:x}",
                    Sha256::digest(format!("{id}-{}", "x".repeat(40)).as_bytes())
                ),
            })
            .into(),
    })
    .unwrap())
}
async fn call(a: &Router, who: &str, method: &str, path: &str, data: Value) -> (StatusCode, Value) {
    let response = a
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {who}-{}", "x".repeat(40)))
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
async fn chat(a: &Router, message: &str, key: &str) -> Value {
    let (status, value) = call(
        a,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":message,"request_id":key}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}
async fn profile(a: &Router) -> Value {
    call(a, "alice", "GET", "/api/collaboration-profile", Value::Null)
        .await
        .1
}
async fn decide(a: &Router, suggestion: &Value, action: &str) -> (StatusCode, Value) {
    call(
        a,
        "alice",
        "POST",
        &format!(
            "/api/profile-suggestions/{}/actions",
            suggestion["id"].as_str().unwrap()
        ),
        json!({"action":action}),
    )
    .await
}

#[tokio::test]
async fn natural_chat_prepares_revises_and_confirms_private_context_without_early_changes() {
    let a = setup();
    let text = "记住：我负责支付服务，回复先给结论";
    let first = chat(&a, text, "remember-1").await;
    assert_eq!(profile(&a).await["revision"], 0);
    let suggestion = &first["profile_suggestion"];
    assert_eq!(
        suggestion["changes"]["private"]["work_context"],
        "我负责支付服务"
    );
    assert_eq!(
        suggestion["changes"]["private"]["response_preferences"],
        "先给结论"
    );
    assert!(suggestion["changes"].get("public").is_none());
    assert!(first["reply"].as_str().unwrap().contains("确认前不会修改"));
    assert_eq!(chat(&a, text, "remember-1").await, first);
    assert!(
        call(&a, "bob", "GET", "/api/profile-suggestions", Value::Null)
            .await
            .1
            .as_array()
            .unwrap()
            .is_empty()
    );
    let changed = chat(&a, "回复改为更简短", "remember-2").await;
    let newer = &changed["profile_suggestion"];
    assert_eq!(
        newer["changes"]["private"]["work_context"],
        "我负责支付服务"
    );
    assert_eq!(
        newer["changes"]["private"]["response_preferences"],
        "更简短"
    );
    assert_eq!(
        decide(&a, suggestion, "confirm").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(profile(&a).await["revision"], 0);
    assert_eq!(decide(&a, newer, "confirm").await.0, StatusCode::OK);
    assert_eq!(decide(&a, newer, "confirm").await.0, StatusCode::OK);
    let saved = profile(&a).await;
    assert_eq!(saved["revision"], 1);
    assert_eq!(saved["private"]["response_preferences"], "更简短");
    assert_eq!(saved["public"]["responsibilities"], "");
}

#[tokio::test]
async fn explicit_public_and_permission_changes_need_separate_owner_confirmation() {
    let a = setup();
    let public = chat(&a, "把我的职责公开为支付服务", "public-1").await;
    assert_eq!(profile(&a).await["public"]["responsibilities"], "");
    assert_eq!(
        decide(&a, &public["profile_suggestion"], "confirm").await.0,
        StatusCode::OK
    );
    assert_eq!(profile(&a).await["public"]["responsibilities"], "支付服务");
    let permission = chat(&a, "允许助手用公开资料答复", "permission-1").await;
    assert_eq!(profile(&a).await["permissions"]["auto_reply_public"], false);
    assert_eq!(
        decide(&a, &permission["profile_suggestion"], "reject")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(profile(&a).await["permissions"]["auto_reply_public"], false);
    let disable = chat(&a, "关闭自动起草", "permission-2").await;
    assert_eq!(
        profile(&a).await["permissions"]["auto_prepare_drafts"],
        true
    );
    assert_eq!(
        decide(&a, &disable["profile_suggestion"], "confirm")
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        profile(&a).await["permissions"]["auto_prepare_drafts"],
        false
    );
}

#[tokio::test]
async fn old_chat_without_request_id_can_still_prepare_a_private_suggestion() {
    let a = setup();
    let (status, response) = call(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({"message":"记住：回复先给结论"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["profile_suggestion"]["status"], "pending");
    assert_eq!(profile(&a).await["revision"], 0);
}
