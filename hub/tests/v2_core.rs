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
        gitlab: GitLabOAuthConfig {
            base_url: gitlab_url.into(),
            client_id: "test-client".into(),
            client_secret: "test-secret".into(),
            callback_path: "/auth/callback".into(),
            session_ttl_seconds: 3600,
        },
        llm: LlmConfig::Stub,
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
}
