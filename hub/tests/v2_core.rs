use agenct_hub::{
    app,
    config::{Config, GitLabOAuthConfig, LlmConfig},
    Hub,
};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{collections::HashSet, path::PathBuf};
use tower::ServiceExt;

fn test_config(gitlab_url: &str) -> Config {
    let mut allowed = HashSet::new();
    allowed.insert(gitlab_url.to_owned());
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        data_dir: PathBuf::from("/tmp/agenct-test"),
        database_path: PathBuf::from(":memory:"),
        public_origin: "http://127.0.0.1:8080".into(),
        local_http: true,
        gitlab: Some(GitLabOAuthConfig {
            base_url: gitlab_url.into(),
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            callback_path: "/auth/callback".into(),
        }),
        local_login: false,
        local_login_token: None,
        session_ttl_seconds: 3600,
        llm: LlmConfig::Stub,
        llm_supports_images: false,
        vision_llm: None,
        gitlab_webhook_secret: Some("wh-secret-1234567890".into()),
        allowed_gitlab_origins: allowed,
        allow_generic_im_webhook: false,
        allow_private_outbound: false,
        admin_usernames: HashSet::from(["alice".into()]),
    }
}

async fn seed_user(
    hub: &Hub,
    id: &str,
    username: &str,
    display_name: &str,
    is_admin: bool,
) -> (String, String) {
    let uid = id.to_owned();
    let uname = username.to_owned();
    let dname = display_name.to_owned();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO users(id, username, display_name, is_admin, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, 'active', 1000)",
                rusqlite::params![uid, uname, dname, i64::from(is_admin)],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    hub.issue_session(id.to_owned(), 3600).await.unwrap()
}

async fn req_json(
    router: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "127.0.0.1:8080");
    if method != "GET" {
        b = b.header(header::ORIGIN, "http://127.0.0.1:8080");
    }
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, format!("agenct-local-session={c}"));
    }
    if let Some(tok) = csrf {
        b = b.header("x-csrf-token", tok);
    }
    let req = if let Some(val) = body {
        b.header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(val.to_string()))
            .unwrap()
    } else {
        b.body(Body::empty()).unwrap()
    };
    let resp = router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

#[tokio::test]
async fn gitlab_oauth_pkce_and_csrf_guard_work_end_to_end() {
    // Spin up a local mock GitLab OAuth server.
    let mock_gitlab = Router::new()
        .route(
            "/oauth/token",
            post(|| async { Json(json!({"access_token": "gl-access-xyz"})) }),
        )
        .route(
            "/api/v4/user",
            get(|| async {
                Json(json!({
                    "id": 42,
                    "username": "alice",
                    "name": "Alice Wang",
                    "state": "active"
                }))
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mock_gitlab).await.unwrap();
    });

    let hub = Hub::open(test_config(&format!("http://{addr}"))).unwrap();
    let router = app(hub);

    // 1. Start login -> redirect + login binding cookie.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auth/login")
                .header(header::HOST, "127.0.0.1:8080")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let location = resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap();
    let loc_url = reqwest::Url::parse(location).unwrap();
    let state = loc_url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    let login_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    // 2. Callback exchanges code and issues session cookie.
    let cb_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/auth/callback?code=abc123&state={state}"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::COOKIE, login_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cb_resp.status(), StatusCode::SEE_OTHER);
    let session_cookie = cb_resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|s| s.starts_with("agenct-local-session="))
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("agenct-local-session=")
        .unwrap()
        .to_owned();

    // 3. Check /auth/session returns authenticated user + CSRF token.
    let (st, sess) = req_json(
        &router,
        "GET",
        "/auth/session",
        Some(&session_cookie),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess["authenticated"], true);
    assert_eq!(sess["user"]["username"], "alice");
    assert_eq!(sess["user"]["is_admin"], true);
    let csrf = sess["csrf_token"].as_str().unwrap();

    // 4. Mutation without CSRF is rejected with 403.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&session_cookie),
        None,
        Some(json!({"message": "hi"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // 5. Mutation with valid CSRF succeeds.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&session_cookie),
        Some(csrf),
        Some(json!({"message": "hello", "request_id": "req-1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(chat["reply"].as_str().unwrap().contains("hello"));

    // 6. Replaying same request_id returns cached reply without duplicating.
    let (st, replay) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&session_cookie),
        Some(csrf),
        Some(json!({"message": "hello", "request_id": "req-1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
}

#[tokio::test]
async fn unified_node_lease_protocol_and_brain_tools_work() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub);

    // Create a cloud node and a pc node.
    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"id": "cloud-1", "name": "Cloud Runner", "kind": "cloud"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();
    assert!(created["install_unix"]
        .as_str()
        .unwrap()
        .contains("http://127.0.0.1:8080/node/install.sh"));

    // Node without protocol 2 header is rejected with 426.
    let bad_proto = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_proto.status(), StatusCode::UPGRADE_REQUIRED);

    // Dispatch a job via single-brain chat (/node stub command).
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node run cargo test"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    // Node polls and receives exclusive lease.
    let work_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(work_resp.status(), StatusCode::OK);
    let work_json: Value =
        serde_json::from_slice(&work_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(work_json["job"]["task_id"], task_id);
    let lease = work_json["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev = work_json["job"]["task_revision"].as_i64().unwrap();

    // Renew lease with live progress.
    let renew = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/node/v1/tasks/{task_id}/lease"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "lease_token": lease,
                        "expected_revision": rev,
                        "stage": "Running tests",
                        "log_tail": "test ok",
                        "elapsed_secs": 3
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(renew.status(), StatusCode::OK);

    // Submit result with PNG evidence attachment + idempotent replay.
    let png_b64 = STANDARD.encode(b"\x89PNG\r\n\x1a\nfake-png-body");
    let submit_payload = json!({
        "lease_token": lease,
        "request_id": "sub-1",
        "expected_revision": rev,
        "status": "completed",
        "content": "All tests passed.",
        "attachments": [{"name": "proof.png", "mime_type": "image/png", "data_base64": png_b64}]
    });
    for _ in 0..2 {
        let sub = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/node/v1/tasks/{task_id}/submit"))
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                    .header("x-agenct-node-protocol", "2")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(submit_payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(sub.status(), StatusCode::OK);
    }

    // Check bootstrap shows completed task & attachment link in message.
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(boot["tasks"][0]["status"], "done");
    assert!(boot["tasks"][0]["result"]
        .as_str()
        .unwrap()
        .contains("/api/attachments/att_"));
}

#[tokio::test]
async fn attachment_acl_and_ssrf_and_automation_permissions_are_enforced() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_c, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    // 1. Attachment ACL: Alice uploads a text attachment; Bob cannot download even after pasting its ID in chat!
    let (st, att) = req_json(
        &router,
        "POST",
        "/api/attachments",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "name": "secret.txt",
            "mime_type": "text/plain",
            "data_base64": STANDARD.encode("top-secret-notes")
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let att_id = att["id"].as_str().unwrap();

    let _ = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"message": format!("Trying to steal {att_id}")})),
    )
    .await;
    let bob_dl = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/attachments/{att_id}"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::COOKIE, format!("agenct-local-session={bob_c}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_dl.status(), StatusCode::NOT_FOUND);

    // 2. SSRF protection: IM webhook rejects loopback/metadata/unallowlisted domains; GitLab rejects arbitrary base_url.
    for bad_url in [
        "http://127.0.0.1:8080/health",
        "http://169.254.169.254/latest/meta-data/",
        "https://evil.example.com/hook",
    ] {
        let (st, _) = req_json(
            &router,
            "PUT",
            "/api/integrations/im",
            Some(&alice_c),
            Some(&alice_csrf),
            Some(json!({"provider": "feishu", "webhook_url": bad_url})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "should reject {bad_url}");
    }

    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/integrations/gitlab",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"base_url": "http://127.0.0.1:9999", "token": "glpat-123"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // 3. Cross-user delegation respects Bob's auto_accept (default false) and grants attachment access.
    let (_, _) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"id": "bob-pc", "name": "Bob PC", "kind": "pc"})),
    )
    .await;

    let (st, del) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": format!("@bob please review {att_id}")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(del["tasks"][0]["mode"], "pending_manual");

    // Now Bob IS explicitly granted access to Alice's delegated attachment!
    let bob_dl_after = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/attachments/{att_id}"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::COOKIE, format!("agenct-local-session={bob_c}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bob_dl_after.status(), StatusCode::OK);

    // 4. Enable auto_accept, verify auto_queued, then revoke auto_accept and verify queued job is marked failed.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/automation/permissions",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"auto_accept": true, "auto_deliver": false, "gitlab_execute": false, "gitlab_reply": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, del2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "@bob auto handle this"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(del2["tasks"][0]["mode"], "auto_queued");
    let auto_tid = del2["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    // Revoke auto_accept -> queued delegation job immediately becomes failed!
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/automation/permissions",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"auto_accept": false, "auto_deliver": false, "gitlab_execute": false, "gitlab_reply": false})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let job_status: String = hub
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT status FROM node_jobs WHERE task_id = ?1",
                [auto_tid],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(job_status, "failed");

    // 5. Append-only audit_logs trigger rejects UPDATE/DELETE.
    let update_err = hub
        .db
        .write(|conn| {
            conn.execute("UPDATE audit_logs SET title='tampered'", [])?;
            Ok(())
        })
        .await;
    assert!(update_err.is_err());
}

#[tokio::test]
async fn gitlab_webhook_requires_auth_and_respects_gitlab_execute_flag() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let _ = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub);

    // Unauthenticated webhook rejected.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/gitlab")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"body": "@alice check build"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Valid secret -> notified_only because gitlab_execute defaults to false.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/gitlab")
                .header(header::HOST, "127.0.0.1:8080")
                .header("x-gitlab-token", "wh-secret-1234567890")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({"body": "@alice check build"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["triggered"][0]["status"], "notified_only");
}

#[tokio::test]
async fn implicit_threads_session_resume_and_rolling_compaction_work() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // 1. Register a PC node.
    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "homelab", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    // 2. Dispatch first task -> automatically binds an implicit thread_id.
    let (st, chat1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node check tmux"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let thread_id = chat1["thread_id"]
        .as_str()
        .expect("implicit thread_id created")
        .to_owned();
    assert!(thread_id.starts_with("thr_"));

    // 3. Node polls work, receives job with thread_id and no session yet, then submits with agent_session_id.
    let poll1 = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(poll1.status(), StatusCode::OK);
    let work1: Value =
        serde_json::from_slice(&poll1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(work1["job"]["thread_id"].as_str(), Some(thread_id.as_str()));
    assert!(work1["job"]["agent_session_id"].is_null());

    let tid1 = work1["job"]["task_id"].as_str().unwrap();
    let lease1 = work1["job"]["lease_token"].as_str().unwrap();
    let rev1 = work1["job"]["task_revision"].as_i64().unwrap();
    let sub1 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/node/v1/tasks/{tid1}/submit"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "lease_token": lease1,
                        "request_id": "sub-1",
                        "expected_revision": rev1,
                        "status": "completed",
                        "content": "agy: 3 windows",
                        "agent_session_id": "ses_homelab_999"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub1.status(), StatusCode::OK);

    // 4. Brain routes follow-up into the same implicit thread -> node receives agent_session_id to resume!
    let (st, chat2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("/node_thread {thread_id} inspect agy pane")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(chat2["thread_id"].as_str(), Some(thread_id.as_str()));

    let poll2 = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let work2: Value =
        serde_json::from_slice(&poll2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(work2["job"]["thread_id"].as_str(), Some(thread_id.as_str()));
    assert_eq!(
        work2["job"]["agent_session_id"].as_str(),
        Some("ses_homelab_999")
    );

    // 5. Rolling context compaction compresses older messages once threshold is exceeded.
    for i in 0..10 {
        let (st, _) = req_json(
            &router,
            "POST",
            "/api/chat",
            Some(&cookie),
            Some(&csrf),
            Some(json!({"message": format!("普通日常对话第 {i} 轮")})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
    }
    let compacted = agenct_hub::brain::compact_user_context(&hub, "u-alice")
        .await
        .unwrap();
    // Background task might already have compacted; either way rolling_summary & watermark must be populated.
    let (summary, watermark) = hub
        .db
        .read(|conn| agenct_hub::brain::read_compaction_state_sync(conn, "u-alice"))
        .await
        .unwrap();
    assert!(compacted || watermark > 0);
    assert!(summary.contains("已压缩历史摘要"));
    assert!(watermark > 0);

    // 6. Bootstrap exposes threads and PUT/DELETE /api/threads/{id} work cleanly.
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(boot["threads"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["id"].as_str() == Some(thread_id.as_str()) && t["has_session"] == true));

    let (st, _) = req_json(
        &router,
        "PUT",
        &format!("/api/threads/{thread_id}"),
        Some(&cookie),
        Some(&csrf),
        Some(json!({"status": "archived"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, _) = req_json(
        &router,
        "DELETE",
        &format!("/api/threads/{thread_id}"),
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

async fn message_rows(hub: &Hub, user_id: &str) -> Vec<(i64, String, String, String, Option<i64>)> {
    let uid = user_id.to_owned();
    hub.db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, role, status, content, reply_to FROM messages WHERE user_id=?1 ORDER BY id",
            )?;
            let rows = stmt
                .query_map([&uid], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn agent_loop_feeds_tool_results_back_to_the_model() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // Query tool without a GitLab PAT: the failure is observed by the model in a second step,
    // which writes the final reply (instead of hard-coded string patching).
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/gitlab 最近提交", "request_id": "req-gl"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let reply = chat["reply"].as_str().unwrap();
    assert!(reply.starts_with("已根据工具结果整理"), "{reply}");
    assert!(reply.contains("fetch_gitlab_commits"), "{reply}");
    assert!(reply.contains("未能拉取 GitLab 提交"), "{reply}");
    assert!(chat["tasks"].as_array().unwrap().is_empty());

    // Dispatch with no registered node: failure is fed back too.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node run cargo test"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let reply = chat["reply"].as_str().unwrap();
    assert!(reply.contains("派发失败"), "{reply}");

    // Every assistant reply is linked to its user message; no pending rows remain.
    let rows = message_rows(&hub, "u-alice").await;
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| r.2 == "complete"));
    assert_eq!(rows[1].4, Some(rows[0].0));
    assert_eq!(rows[3].4, Some(rows[2].0));
}

#[tokio::test]
async fn failed_turns_are_marked_failed_retryable_and_replay_uses_reply_link() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // LLM failure: user message is kept but marked failed, no assistant reply is stored.
    for _ in 0..2 {
        let (st, _) = req_json(
            &router,
            "POST",
            "/api/chat",
            Some(&cookie),
            Some(&csrf),
            Some(json!({"message": "/stub_fail", "request_id": "req-f"})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_GATEWAY);
    }
    let rows = message_rows(&hub, "u-alice").await;
    assert_eq!(rows.len(), 1, "retry must reuse the failed row");
    assert_eq!(rows[0].1, "user");
    assert_eq!(rows[0].2, "failed");

    // Retrying the same request_id with a working turn revives the same row.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "hello again", "request_id": "req-f"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let reply = chat["reply"].as_str().unwrap().to_owned();
    let rows = message_rows(&hub, "u-alice").await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].2.as_str(), rows[0].3.as_str()),
        ("complete", "hello again")
    );
    assert_eq!(rows[1].4, Some(rows[0].0));

    // An unrelated assistant message (e.g. a node result) lands afterwards; replay must still
    // return the linked reply, not "the next assistant message".
    hub.db
        .write(|conn| {
            conn.execute(
                "INSERT INTO messages(user_id, role, content, status, created_at)
                 VALUES ('u-alice', 'assistant', 'node result noise', 'complete', 1)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, replay) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "hello again", "request_id": "req-f"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["reply"].as_str().unwrap(), reply);

    // Failed turns never leak into the model's history.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/stub_fail", "request_id": "req-f2"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert!(chat["error"].is_string());
    let (_, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    let last = boot["messages"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["status"], "failed");
}

#[derive(Debug)]
struct CronRow {
    last_status: String,
    last_error: String,
    fail_count: i64,
    next_retry_at: Option<i64>,
}

async fn cron_row(hub: &Hub, id: &str) -> CronRow {
    let id = id.to_owned();
    hub.db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT last_status, last_error, fail_count, next_retry_at FROM cron_jobs WHERE id=?1",
                [&id],
                |r| {
                    Ok(CronRow {
                        last_status: r.get(0)?,
                        last_error: r.get(1)?,
                        fail_count: r.get(2)?,
                        next_retry_at: r.get(3)?,
                    })
                },
            )?)
        })
        .await
        .unwrap()
}

async fn create_due_cron(
    router: &Router,
    hub: &Hub,
    cookie: &str,
    csrf: &str,
    instruction: &str,
    target: &str,
) -> String {
    let (st, cron) = req_json(
        router,
        "POST",
        "/api/automation/crons",
        Some(cookie),
        Some(csrf),
        Some(json!({"interval_minutes": 60, "instruction": instruction, "target": target})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let id = cron["id"].as_str().unwrap().to_owned();
    let id_db = id.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "UPDATE cron_jobs SET created_at = 0 WHERE id = ?1",
                [&id_db],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    id
}

async fn force_retry_now(hub: &Hub, id: &str) {
    let id = id.to_owned();
    hub.db
        .write(move |conn| {
            conn.execute(
                "UPDATE cron_jobs SET next_retry_at = 1 WHERE id = ?1 AND next_retry_at IS NOT NULL",
                [&id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

#[test]
fn cron_retry_backoff_is_exponential_and_capped() {
    use agenct_hub::automation::retry_delay_secs;
    assert_eq!(retry_delay_secs(1), 60);
    assert_eq!(retry_delay_secs(2), 120);
    assert_eq!(retry_delay_secs(3), 240);
    assert_eq!(retry_delay_secs(10), 1800);
}

#[tokio::test]
async fn cloud_cron_runs_through_agent_loop_with_restricted_tools() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // No keyword routing: the brain calls fetch_gitlab_commits and reports on the real result.
    let gl = create_due_cron(
        &router,
        &hub,
        &cookie,
        &csrf,
        "/gitlab 每日提交汇总",
        "cloud",
    )
    .await;
    // A cron must not be able to create more crons.
    let sneaky = create_due_cron(&router, &hub, &cookie, &csrf, "/cron 再建一个", "cloud").await;

    assert_eq!(
        agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
        2
    );
    // Already ran: nothing is due right after.
    assert_eq!(
        agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
        0
    );

    for id in [&gl, &sneaky] {
        let row = cron_row(&hub, id).await;
        assert_eq!(row.last_status, "ok", "{row:?}");
        assert_eq!(row.fail_count, 0);
    }
    let rows = message_rows(&hub, "u-alice").await;
    assert!(
        rows.iter().all(|r| r.1 == "assistant"),
        "cron must not fake user messages"
    );
    let gl_report = rows
        .iter()
        .find(|r| r.3.contains("每日提交汇总"))
        .expect("gitlab report");
    assert!(gl_report.3.starts_with("**[定时汇报]**"));
    assert!(
        gl_report.3.contains("已根据工具结果整理"),
        "{}",
        gl_report.3
    );
    assert!(gl_report.3.contains("fetch_gitlab_commits"));
    let sneaky_report = rows.iter().find(|r| r.3.contains("再建一个")).unwrap();
    assert!(
        sneaky_report.3.contains("manage_cron"),
        "{}",
        sneaky_report.3
    );

    let crons: i64 = hub
        .db
        .read(|conn| Ok(conn.query_row("SELECT count(*) FROM cron_jobs", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(crons, 2, "blocked manage_cron must not add a cron");
}

#[tokio::test]
async fn failing_crons_back_off_retry_and_give_up_once() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let id = create_due_cron(&router, &hub, &cookie, &csrf, "/stub_fail", "cloud").await;
    let before = agenct_hub::config::now_unix();
    assert_eq!(
        agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
        1
    );
    let row = cron_row(&hub, &id).await;
    assert_eq!((row.last_status.as_str(), row.fail_count), ("failed", 1));
    assert!(!row.last_error.is_empty());
    let retry_at = row.next_retry_at.expect("retry scheduled");
    assert!(retry_at >= before + 60 && retry_at <= before + 62);

    // Not due again until the backoff elapses.
    assert_eq!(
        agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
        0
    );

    for expected in 2..=4 {
        force_retry_now(&hub, &id).await;
        assert_eq!(
            agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
            1
        );
        assert_eq!(cron_row(&hub, &id).await.fail_count, expected);
    }
    let row = cron_row(&hub, &id).await;
    assert_eq!(row.next_retry_at, None, "retries exhausted");
    let give_ups = message_rows(&hub, "u-alice")
        .await
        .into_iter()
        .filter(|r| r.3.starts_with("**[定时任务失败]**"))
        .count();
    assert_eq!(give_ups, 1);

    // Node-targeted cron with no registered node fails (and is retried) instead of silently vanishing.
    let node_cron = create_due_cron(&router, &hub, &cookie, &csrf, "跑一下备份", "auto").await;
    agenct_hub::automation::run_due_crons(&hub).await.unwrap();
    let row = cron_row(&hub, &node_cron).await;
    assert_eq!(row.last_status, "failed");
    assert!(row.last_error.contains("没有可用的执行节点"), "{row:?}");
    assert!(row.next_retry_at.is_some());
}

async fn upload_png(router: &Router, cookie: &str, csrf: &str) -> String {
    let png = STANDARD.encode(b"\x89PNG\r\n\x1a\nfake-screenshot");
    let (st, att) = req_json(
        router,
        "POST",
        "/api/attachments",
        Some(cookie),
        Some(csrf),
        Some(json!({"name": "shot.png", "mime_type": "image/png", "data_base64": png})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{att}");
    att["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn users_configure_their_own_brain_and_vision_models_safely() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, cur) = req_json(
        &router,
        "GET",
        "/api/settings/models",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(cur["brain"].is_null());
    assert_eq!(cur["server_default"]["brain_model"], "stub");

    // SSRF: private / plain-http / non-chat-completions URLs are rejected.
    for url in [
        "https://10.0.0.11/v1/chat/completions",
        "https://127.0.0.1/v1/chat/completions",
        "http://1.1.1.1/v1/chat/completions",
        "https://1.1.1.1/v1/models",
    ] {
        let (st, err) = req_json(
            &router,
            "PUT",
            "/api/settings/models",
            Some(&cookie),
            Some(&csrf),
            Some(json!({"brain": {"url": url, "model": "m", "api_key": "sk-test-123456789"}})),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{url}: {err}");
    }
    // Key is required the first time.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"brain": {"url": "https://1.1.1.1/v1/chat/completions", "model": "m"}})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, saved) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "brain": {"url": "https://1.1.1.1/v1/chat/completions", "model": "qwen-vl-max",
                      "api_key": "sk-secret-abcdef123456", "supports_images": true},
            "vision": {"url": "https://1.1.1.1/v1/chat/completions", "model": "glm-4v", "api_key": "sk-vision-999999999"}
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    assert_eq!(saved["brain"]["model"], "qwen-vl-max");
    assert_eq!(saved["brain"]["supports_images"], true);
    assert_eq!(saved["brain"]["key_preview"], "****3456");
    assert!(
        !saved.to_string().contains("sk-secret"),
        "API key must never be echoed"
    );

    // Updating without a key keeps the stored key.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"brain": {"url": "https://1.1.1.1/v1/chat/completions", "model": "qwen-max"}})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let stored = agenct_hub::models::load(&hub, "u-alice").await.unwrap();
    let brain = stored.brain.unwrap();
    assert_eq!(
        (brain.model.as_str(), brain.api_key.as_str()),
        ("qwen-max", "sk-secret-abcdef123456")
    );
    assert!(!brain.supports_images);

    // Clearing falls back to the server default.
    let (st, cleared) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"clear_brain": true, "clear_vision": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(cleared["brain"].is_null() && cleared["vision"].is_null());
    let (st, probe) = req_json(
        &router,
        "POST",
        "/api/settings/models/test",
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(probe["brain"]["ok"], true);
    assert!(probe["vision"].is_null());
}

#[tokio::test]
async fn images_reach_the_brain_inline_via_captions_or_with_an_honest_note() {
    // 1. Text-only brain, no vision model: the brain is told it cannot see the image.
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());
    let att = upload_png(&router, &cookie, &csrf).await;
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("看下这个 ![shot.png](/api/attachments/{att})")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        chat["reply"].as_str().unwrap().contains("没有配置识图模型"),
        "{chat}"
    );

    // 2. Vision model configured: caption is injected and persisted for follow-up turns.
    let mut cfg = test_config("https://git.example.com");
    cfg.vision_llm = Some(LlmConfig::Stub);
    let hub = Hub::open(cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());
    let att = upload_png(&router, &cookie, &csrf).await;
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("看下这个 ![shot.png](/api/attachments/{att})")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        chat["reply"]
            .as_str()
            .unwrap()
            .contains("connection refused"),
        "{chat}"
    );
    let context: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT context FROM messages WHERE role='user' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(context.contains("【图片 shot.png 内容"), "{context}");

    // 3. Image-capable brain: images are sent inline, no captioning.
    let mut cfg = test_config("https://git.example.com");
    cfg.llm_supports_images = true;
    let hub = Hub::open(cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());
    let att = upload_png(&router, &cookie, &csrf).await;
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("![shot.png](/api/attachments/{att})")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        chat["reply"]
            .as_str()
            .unwrap()
            .starts_with("（看到 1 张图片）"),
        "{chat}"
    );
}

fn local_only_config() -> Config {
    let mut cfg = test_config("https://git.example.com");
    cfg.gitlab = None;
    cfg.local_login = true;
    cfg.local_login_token = Some("local-test-token-0123456789".into());
    cfg
}

async fn post_local_login(
    router: &Router,
    token: &str,
    origin: Option<&str>,
) -> (StatusCode, Option<String>, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri("/auth/local")
        .header(header::HOST, "127.0.0.1:8080")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(o) = origin {
        b = b.header(header::ORIGIN, o);
    }
    let resp = router
        .clone()
        .oneshot(
            b.body(Body::from(json!({"token": token}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let cookie = resp
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|c| c.strip_prefix("agenct-local-session="))
        .map(|c| c.split(';').next().unwrap_or("").to_owned())
        .filter(|c| !c.is_empty());
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        cookie,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn local_token_login_works_without_gitlab() {
    let hub = Hub::open(local_only_config()).unwrap();
    let router = app(hub.clone());

    // Login page advertises local login only; GitLab endpoints are disabled.
    let (st, sess) = req_json(&router, "GET", "/auth/session", None, None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess["login"], json!({"gitlab": false, "local": true}));
    let (st, _) = req_json(&router, "GET", "/auth/login", None, None, None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    // Cross-origin / missing Origin is rejected (login CSRF), wrong token is rejected.
    let (st, _, _) = post_local_login(&router, "local-test-token-0123456789", None).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, _, _) = post_local_login(
        &router,
        "local-test-token-0123456789",
        Some("https://evil.example"),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, cookie, _) = post_local_login(
        &router,
        "wrong-token-000000000000",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert!(cookie.is_none());

    // Correct token -> session cookie for the local owner (admin), usable for the app.
    let (st, cookie, body) = post_local_login(
        &router,
        "local-test-token-0123456789",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cookie = cookie.expect("session cookie");
    let csrf = body["csrf_token"].as_str().unwrap().to_owned();
    let (st, sess) = req_json(&router, "GET", "/auth/session", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess["authenticated"], true);
    assert_eq!(sess["user"]["id"], "local-owner");
    assert_eq!(sess["user"]["is_admin"], true);
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "hello"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chat}");
}

#[tokio::test]
async fn local_login_is_off_by_default_when_gitlab_is_configured() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let router = app(hub);
    let (st, sess) = req_json(&router, "GET", "/auth/session", None, None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(sess["login"], json!({"gitlab": true, "local": false}));
    let (st, _, _) = post_local_login(
        &router,
        "anything-at-all-123456",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[test]
fn generated_local_token_is_persisted_hashed_and_rotatable() {
    let dir = std::env::temp_dir().join(format!("agenct-local-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = local_only_config();
    cfg.local_login_token = None;
    cfg.data_dir = dir.clone();
    cfg.database_path = dir.join("agenct.sqlite3");
    let db = agenct_hub::db::Db::open(&cfg.database_path).unwrap();

    let (token, path) = db
        .write_blocking(|c| agenct_hub::auth::init_local_login(c, &cfg))
        .unwrap()
        .expect("first start generates a token");
    assert!(token.starts_with("agl_") && token.len() == 68);
    let saved = std::fs::read_to_string(path.unwrap()).unwrap();
    assert_eq!(saved.trim(), token);
    // Second start does not regenerate.
    assert!(db
        .write_blocking(|c| agenct_hub::auth::init_local_login(c, &cfg))
        .unwrap()
        .is_none());
    // Only the hash is stored.
    let stored: String = db
        .write_blocking(|c| Ok(c.query_row("SELECT token_hash FROM local_auth", [], |r| r.get(0))?))
        .unwrap();
    assert_ne!(stored, token);
    assert_eq!(stored, agenct_hub::auth::sha256_hex(&token));
    // Rotation replaces it.
    let (rotated, _) = db
        .write_blocking(|c| agenct_hub::auth::rotate_local_token(c, &cfg))
        .unwrap();
    assert_ne!(rotated, token);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}
