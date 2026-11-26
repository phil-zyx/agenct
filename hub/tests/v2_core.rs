use agenct_hub::{
    app,
    config::{Config, GitLabOAuthConfig, LlmConfig},
    Hub,
};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
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
        master_key: None,
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

    let events = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::COOKIE, format!("agenct-local-session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(events.status(), StatusCode::OK);
    let mut events = events.into_body();
    let connected = events.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(connected.starts_with(b"event: connected\n"));

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
    let progress = tokio::time::timeout(std::time::Duration::from_secs(1), events.frame())
        .await
        .expect("lease renewal emits SSE progress")
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap();
    let frame = std::str::from_utf8(&progress).unwrap();
    let data = frame
        .strip_prefix("event: task\ndata: ")
        .unwrap()
        .trim_end();
    assert_eq!(
        serde_json::from_str::<Value>(data).unwrap(),
        json!({
            "task_id": task_id,
            "status": "running",
            "stage": "Running tests",
            "log_tail": "test ok",
            "elapsed_secs": 3
        })
    );

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

#[tokio::test]
async fn node_submit_prevalidates_lease_and_replay_before_storing_attachments() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-1", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node capture screenshot"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

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
    let lease = work_json["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev = work_json["job"]["task_revision"].as_i64().unwrap();

    let png_b64 = STANDARD.encode(b"\x89PNG\r\n\x1a\nfake-evidence");
    let att_count = || {
        let db = hub.db.clone();
        async move {
            db.read(|c| {
                Ok(c.query_row("SELECT count(*) FROM attachments", [], |r| {
                    r.get::<_, i64>(0)
                })?)
            })
            .await
            .unwrap()
        }
    };

    // Invalid lease must be rejected with 409 without persisting any attachment.
    let bad_submit = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/node/v1/tasks/{task_id}/submit"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "lease_token": "wrong-lease-token",
                        "request_id": "sub-bad",
                        "expected_revision": rev,
                        "status": "completed",
                        "content": "Done",
                        "attachments": [{"name": "ev.png", "mime_type": "image/png", "data_base64": png_b64}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(bad_submit.status(), StatusCode::CONFLICT);
    assert_eq!(att_count().await, 0);

    // Valid submit stores exactly one attachment; idempotent replay stores zero extra copies.
    let payload = json!({
        "lease_token": lease,
        "request_id": "sub-ok",
        "expected_revision": rev,
        "status": "completed",
        "content": "Captured.",
        "attachments": [{"name": "ev.png", "mime_type": "image/png", "data_base64": png_b64}]
    })
    .to_string();
    for _ in 0..2 {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/node/v1/tasks/{task_id}/submit"))
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                    .header("x-agenct-node-protocol", "2")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    assert_eq!(att_count().await, 1);
}

#[tokio::test]
async fn manual_deliver_invalidates_active_node_lease_and_cancel_notifies_peer() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (alice_cookie, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_cookie, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&alice_cookie),
        Some(&alice_csrf),
        Some(json!({"name": "pc-1", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    // Queue and claim a task on Alice's node, then manually deliver it.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_cookie),
        Some(&alice_csrf),
        Some(json!({"message": "/node build project"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

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
    let work_json: Value =
        serde_json::from_slice(&work_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let lease = work_json["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev = work_json["job"]["task_revision"].as_i64().unwrap();

    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_cookie),
        Some(&alice_csrf),
        Some(json!({"action": "deliver", "result": "Manual answer"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Old node lease must now be rejected with 409 Conflict.
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
                    json!({"lease_token": lease, "expected_revision": rev}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(renew.status(), StatusCode::CONFLICT);

    // Cross-user delegation: Bob cancels -> Alice receives SSE event, and delivering a canceled task is rejected.
    let (_, delegated_id, _) =
        agenct_hub::collab::delegate_to_peer(&hub, "u-alice", "bob", "check logs")
            .await
            .unwrap();
    let mut alice_rx = hub.events.subscribe("u-alice");
    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{delegated_id}/action"),
        Some(&bob_cookie),
        Some(&bob_csrf),
        Some(json!({"action": "cancel"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let ev = alice_rx.try_recv().expect("requester notified on cancel");
    assert_eq!(ev.kind, "task");
    assert!(ev.data.contains(&delegated_id));

    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{delegated_id}/action"),
        Some(&bob_cookie),
        Some(&bob_csrf),
        Some(json!({"action": "deliver", "result": "too late"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn node_long_poll_wakes_immediately_on_enqueue() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-fast", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    let poll_router = router.clone();
    let poll_token = node_token.clone();
    let poll_handle = tokio::spawn(async move {
        poll_router
            .oneshot(
                Request::builder()
                    .uri("/node/v1/work?wait=10")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {poll_token}"))
                    .header("x-agenct-node-protocol", "2")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
    });

    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node ping"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    let work_resp = tokio::time::timeout(std::time::Duration::from_secs(2), poll_handle)
        .await
        .expect("long-poll must wake well before 10s timeout")
        .unwrap();
    assert_eq!(work_resp.status(), StatusCode::OK);
    let work_json: Value =
        serde_json::from_slice(&work_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(work_json["job"]["task_id"].as_str(), Some(task_id.as_str()));
    assert!(work_json["job"]["lease_token"]
        .as_str()
        .is_some_and(|t| !t.is_empty()));
}

#[tokio::test]
async fn repo_aware_routing_picks_online_node_matching_repo() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st1, n1) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "FrontNode", "kind": "pc"})),
    )
    .await;
    assert_eq!(st1, StatusCode::OK);
    let tok1 = n1["token"].as_str().unwrap().to_owned();

    let (st2, n2) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "BackNode", "kind": "pc"})),
    )
    .await;
    assert_eq!(st2, StatusCode::OK);
    let tok2 = n2["token"].as_str().unwrap().to_owned();
    let back_id = n2["node"]["id"].as_str().unwrap().to_owned();

    for (tok, env_hdr) in [
        (&tok1, "kind:pc | os:linux | agent:codex | repos:frontend"),
        (
            &tok2,
            "kind:pc | os:linux | agent:codex | repos:billing-api,agenct",
        ),
    ] {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/v1/work")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                    .header("x-agenct-node-protocol", "2")
                    .header("x-agenct-node-env", env_hdr)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node inspect agenct repo status"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(chat["tasks"][0]["node_id"].as_str(), Some(back_id.as_str()));

    let claim = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {tok2}"))
                .header("x-agenct-node-protocol", "2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(claim.status(), StatusCode::OK);
    let claim_json: Value =
        serde_json::from_slice(&claim.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        claim_json["job"]["task_id"].as_str(),
        chat["tasks"][0]["task_id"].as_str()
    );
}

#[tokio::test]
async fn native_tool_calling_streaming_and_json_fallback_work() {
    let mock_calls = Arc::new(AtomicUsize::new(0));
    let mock_calls_stream = mock_calls.clone();
    let mock_app = Router::new()
        .route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let calls = mock_calls_stream.clone();
                async move {
                    let step = calls.fetch_add(1, Ordering::SeqCst);
                    if step == 0 {
                        assert_eq!(body["stream"], true);
                        assert!(body["tools"].is_array());
                        let chunks = vec![
                            Ok::<_, std::convert::Infallible>(
                                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_mem_1\",\"type\":\"function\",\"function\":{\"name\":\"update_memory\",\"arguments\":\"{\\\"memory_ap\"}}]}}]}\n\n".to_owned(),
                            ),
                            Ok(
                                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"pend\\\":\\\"偏爱简洁直接的汇报\\\"}\"}},{\"index\":1,\"id\":\"call_gl_1\",\"type\":\"function\",\"function\":{\"name\":\"fetch_gitlab_commits\",\"arguments\":\"{}\"}}]}}]}\n\n".to_owned(),
                            ),
                            Ok("data: [DONE]\n\n".to_owned()),
                        ];
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from_stream(futures_util::stream::iter(chunks)))
                            .unwrap()
                    } else {
                        let msgs = body["messages"].as_array().unwrap();
                        assert!(msgs
                            .iter()
                            .any(|m| m["role"] == "assistant" && m["tool_calls"].is_array()));
                        assert!(msgs
                            .iter()
                            .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_gl_1"));
                        let chunks = vec![
                            Ok::<_, std::convert::Infallible>(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"<thi\"}}]}\n\n"
                                    .to_owned(),
                            ),
                            Ok(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"nk>hidden\"}}]}\n\n"
                                    .to_owned(),
                            ),
                            Ok(
                                "data: {\"choices\":[{\"delta\":{\"content\":\" reasoning</think>已更新记忆并\"}}]}\n\n"
                                    .to_owned(),
                            ),
                            Ok(
                                "data: {\"choices\":[{\"delta\":{\"content\":\"完成汇总！\"}}]}\n\n"
                                    .to_owned(),
                            ),
                            Ok("data: [DONE]\n\n".to_owned()),
                        ];
                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "text/event-stream")
                            .body(Body::from_stream(futures_util::stream::iter(chunks)))
                            .unwrap()
                    }
                }
            }),
        )
        .route(
            "/v1/fallback/chat/completions",
            post(|Json(body): Json<Value>| async move {
                if body.get("tools").is_some() {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": {"message": "tools unsupported"}})),
                    )
                        .into_response();
                }
                assert_eq!(body["response_format"]["type"], "json_object");
                (
                    StatusCode::OK,
                    Json(json!({
                        "choices": [{
                            "message": {
                                "content": "{\"reply\":\"兼容模式成功\",\"tools\":[]}"
                            }
                        }]
                    })),
                )
                    .into_response()
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, mock_app).await.unwrap();
    });

    let mut cfg = test_config("https://git.example.com");
    cfg.allow_private_outbound = true;
    cfg.llm = LlmConfig::OpenAi {
        url: format!("http://127.0.0.1:{}/v1/chat/completions", addr.port()),
        api_key: "sk-mock-123456".into(),
        model: "gpt-test".into(),
    };
    let hub = Hub::open(cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let mut rx = hub.events.subscribe("u-alice");
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "记住我偏爱简洁直接的汇报，并查下提交"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chat}");
    assert_eq!(chat["reply"], "已更新记忆并完成汇总！");
    assert_eq!(mock_calls.load(Ordering::SeqCst), 2);

    let mut streamed = String::new();
    let mut deltas = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if ev.kind == "delta" {
            let v: Value = serde_json::from_str(&ev.data).unwrap();
            if let Some(txt) = v["text"].as_str() {
                streamed.push_str(txt);
            }
            deltas.push(v);
        }
    }
    assert!(!deltas.is_empty(), "expected streaming delta events");
    assert!(!streamed.contains("hidden") && !streamed.contains("<think>"));
    assert_eq!(streamed, "已更新记忆并完成汇总！");
    assert_eq!(deltas.last().unwrap()["done"], true);

    let memory: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT memory_md FROM agent_profiles WHERE user_id='u-alice'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(memory.contains("偏爱简洁直接的汇报"), "{memory}");

    // Scenario B: upstream rejects `tools` with 400 -> automatic fallback to JSON-envelope mode.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "brain": {
                "url": format!("http://127.0.0.1:{}/v1/fallback/chat/completions", addr.port()),
                "model": "legacy-json-model",
                "api_key": "sk-legacy-123456"
            }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, fallback_chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "你好"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{fallback_chat}");
    assert_eq!(fallback_chat["reply"], "兼容模式成功");

    server.abort();
}

#[tokio::test]
async fn message_pagination_thread_filter_and_fts5_search_work() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let _ = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    hub.db
        .write(|conn| {
            conn.execute(
                "INSERT INTO threads(id, user_id, title, status, summary, turn_count, updated_at)
                 VALUES ('thr_alpha', 'u-alice', '数据库排查', 'active', '', 2, 100)",
                [],
            )?;
            for (idx, (uid, thr, content)) in [
                ("u-alice", Some("thr_alpha"), "请排查数据库内存泄漏问题"),
                ("u-alice", None, "检查 err-502 错误日志"),
                ("u-alice", Some("thr_alpha"), "DB 连接池调优建议"),
                ("u-alice", None, "第四条日常对话"),
                ("u-alice", None, "第五条收尾记录"),
                ("u-bob", None, "数据库内存机密属于Bob"),
            ]
            .into_iter()
            .enumerate()
            {
                conn.execute(
                    "INSERT INTO messages(user_id, thread_id, role, content, status, created_at)
                     VALUES (?1, ?2, 'user', ?3, 'complete', ?4)",
                    rusqlite::params![uid, thr, content, 1000 + idx as i64],
                )?;
            }
            conn.execute(
                "INSERT INTO ledgers(id, user_id, title, status, notes, updated_at)
                 VALUES ('ldg_db', 'u-alice', '数据库容量治理', 'active', '清理旧索引', 1000)",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/brain",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"memory_md": "- 偏好 DB 参数调优\n- 喜欢先看结论", "avatar_id": "buddy"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&alice_c), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(boot["profile"]["avatar_id"], "buddy");
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/brain",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"avatar_id": "invalid_avatar"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // 1. Cursor pagination: page 1 (2 newest), page 2 (next 2), page 3 (last 1).
    let (st, p1) = req_json(
        &router,
        "GET",
        "/api/messages?limit=2",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let m1 = p1["messages"].as_array().unwrap();
    assert_eq!(m1.len(), 2);
    assert_eq!(m1[0]["content"], "第四条日常对话");
    assert_eq!(m1[1]["content"], "第五条收尾记录");
    assert_eq!(p1["has_more"], true);
    let before1 = p1["next_before_id"].as_i64().unwrap();

    let (st, p2) = req_json(
        &router,
        "GET",
        &format!("/api/messages?before_id={before1}&limit=2"),
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let m2 = p2["messages"].as_array().unwrap();
    assert_eq!(m2.len(), 2);
    assert_eq!(m2[0]["content"], "检查 err-502 错误日志");
    assert_eq!(m2[1]["content"], "DB 连接池调优建议");
    assert_eq!(p2["has_more"], true);
    let before2 = p2["next_before_id"].as_i64().unwrap();

    let (st, p3) = req_json(
        &router,
        "GET",
        &format!("/api/messages?before_id={before2}&limit=10"),
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let m3 = p3["messages"].as_array().unwrap();
    assert_eq!(m3.len(), 1);
    assert_eq!(m3[0]["content"], "请排查数据库内存泄漏问题");
    assert_eq!(p3["has_more"], false);

    // 2. Thread filter returns only thr_alpha messages.
    let (st, tf) = req_json(
        &router,
        "GET",
        "/api/messages?thread_id=thr_alpha",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let tm = tf["messages"].as_array().unwrap();
    assert_eq!(tm.len(), 2);
    assert!(tm.iter().all(|m| m["thread_id"] == "thr_alpha"));

    // 3. FTS5 trigram search (>=3 chars), hyphenated term, and <3-char fallback, strictly user-scoped.
    let (st, s_zh) = req_json(
        &router,
        "GET",
        "/api/search?q=%E6%95%B0%E6%8D%AE%E5%BA%93",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s_zh["messages"].as_array().unwrap().len(), 1);
    assert!(s_zh["messages"][0]["snippet"]
        .as_str()
        .unwrap()
        .contains("数据库内存泄漏"));
    assert_eq!(s_zh["ledgers"].as_array().unwrap().len(), 1);
    assert_eq!(s_zh["ledgers"][0]["id"], "ldg_db");

    let (st, s_hyph) = req_json(
        &router,
        "GET",
        "/api/search?q=err-502",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s_hyph["messages"].as_array().unwrap().len(), 1);

    let (st, s_short) = req_json(
        &router,
        "GET",
        "/api/search?q=DB",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(s_short["messages"].as_array().unwrap().len(), 1);
    assert_eq!(s_short["memory_matches"].as_array().unwrap().len(), 1);
    assert!(s_short["memory_matches"][0]
        .as_str()
        .unwrap()
        .contains("DB 参数调优"));

    // 4. Both /api/bootstrap and /api/messages expose request_id for client optimistic deduplication.
    let (st, chat_resp) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "check req id", "request_id": "req-dedup-test-1"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chat_resp}");
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&alice_c), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(boot["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["request_id"] == "req-dedup-test-1"));
    let (st, page) = req_json(
        &router,
        "GET",
        "/api/messages?limit=5",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["request_id"] == "req-dedup-test-1"));
}

#[tokio::test]
async fn async_task_callback_feeds_node_result_back_to_brain_with_depth_cap() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-cb", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node_cb 验收测试结果并更新账本 :: run tests"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    let (on_cb, cb_depth): (String, i64) = hub
        .db
        .read({
            let tid = task_id.clone();
            move |conn| {
                Ok(conn.query_row(
                    "SELECT on_complete, callback_depth FROM tasks WHERE id = ?1",
                    [&tid],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            }
        })
        .await
        .unwrap();
    assert_eq!(on_cb, "验收测试结果并更新账本");
    assert_eq!(cb_depth, 0);

    let poll = router
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
    let work_json: Value =
        serde_json::from_slice(&poll.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let lease = work_json["job"]["lease_token"].as_str().unwrap();
    let rev = work_json["job"]["task_revision"].as_i64().unwrap();

    let mut rx = hub.events.subscribe("u-alice");
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
                .body(Body::from(
                    json!({
                        "lease_token": lease,
                        "request_id": "sub-cb-1",
                        "expected_revision": rev,
                        "status": "completed",
                        "content": "All 25 tests passed."
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub.status(), StatusCode::OK);

    // Wait for async brain callback completion event.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let ev = rx.recv().await.unwrap();
            if ev.kind == "message" && ev.data.contains("\"callback\":true") {
                break;
            }
        }
    })
    .await
    .expect("async task callback should emit message event");

    let (msgs, ledgers, audit_cnt, saved_result): (Vec<String>, Vec<String>, i64, Option<String>) =
        hub.db
            .read({
                let tid = task_id.clone();
                move |conn| {
                    let mut ms = conn.prepare(
                        "SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id",
                    )?;
                    let msgs = ms
                        .query_map([], |r| r.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?;
                    let mut ls = conn.prepare("SELECT title FROM ledgers WHERE user_id='u-alice'")?;
                    let ledgers = ls
                        .query_map([], |r| r.get(0))?
                        .collect::<rusqlite::Result<Vec<String>>>()?;
                    let audit_cnt: i64 = conn.query_row(
                        "SELECT count(*) FROM audit_logs WHERE user_id='u-alice' AND action='task_callback' AND status='ok'",
                        [],
                        |r| r.get(0),
                    )?;
                    let saved_result: Option<String> = conn.query_row(
                        "SELECT result FROM tasks WHERE id=?1",
                        [&tid],
                        |r| r.get(0),
                    )?;
                    Ok((msgs, ledgers, audit_cnt, saved_result))
                }
            })
            .await
            .unwrap();

    assert_eq!(saved_result.as_deref(), Some("All 25 tests passed."));
    assert!(msgs.iter().any(
        |m| m.contains("**[自动跟进 · run tests]**") && m.contains("已验收节点结果并完成跟进")
    ));
    // Tasks with `on_complete` must not spam chat with a duplicate raw node essay alongside the callback digest.
    assert!(!msgs.iter().any(|m| m.starts_with("**[节点 ")));
    assert!(ledgers.iter().any(|t| t == "自动跟进事项"));
    assert_eq!(audit_cnt, 1);

    // Evidence attachment links from node output are preserved inside the concise callback message.
    agenct_hub::brain::run_task_callback(
        hub.clone(),
        "u-alice".into(),
        task_id.clone(),
        "capture ui".into(),
        "pc-cb".into(),
        "done".into(),
        "Captured.\n\n![ev](/api/attachments/att_0123456789abcdef0123456789abcdef)".into(),
        "总结截图结论".into(),
        None,
        0,
    )
    .await;
    let att_msg: String = hub
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(att_msg.contains("**[自动跟进 · capture ui]**"));
    assert!(att_msg.contains("![ev](/api/attachments/att_0123456789abcdef0123456789abcdef)"));

    // Depth cap: running callback at MAX_CALLBACK_DEPTH must be a no-op.
    let before_msgs: i64 = hub
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE user_id='u-alice'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    agenct_hub::brain::run_task_callback(
        hub.clone(),
        "u-alice".into(),
        task_id,
        "run tests".into(),
        "pc-cb".into(),
        "done".into(),
        "ok".into(),
        "再触发".into(),
        None,
        agenct_hub::brain::MAX_CALLBACK_DEPTH,
    )
    .await;
    let after_msgs: i64 = hub
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM messages WHERE user_id='u-alice'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(after_msgs, before_msgs);

    // Callback failure fallback: if the brain LLM fails during callback, post the raw node output instead of dropping it.
    let dead_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead_listener.local_addr().unwrap();
    drop(dead_listener);
    let mut err_cfg = test_config("https://git.example.com");
    err_cfg.allow_private_outbound = true;
    let err_hub = Hub::open(err_cfg).unwrap();
    let (err_c, err_csrf) = seed_user(&err_hub, "u-alice", "alice", "Alice", true).await;
    let err_router = app(err_hub.clone());
    let (st, _) = req_json(
        &err_router,
        "PUT",
        "/api/settings/models",
        Some(&err_c),
        Some(&err_csrf),
        Some(json!({
            "brain": {
                "url": format!("http://{dead_addr}/v1/chat/completions"),
                "model": "broken-brain",
                "api_key": "sk-broken"
            }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    agenct_hub::brain::run_task_callback(
        err_hub.clone(),
        "u-alice".into(),
        "tsk_fb".into(),
        "fallback task".into(),
        "pc-cb (codex)".into(),
        "done".into(),
        "RAW_NODE_RESULT_SAFE".into(),
        "总结".into(),
        None,
        0,
    )
    .await;
    let fb_msg: String = err_hub
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(fb_msg.contains("**[节点 pc-cb (codex) · fallback task]**"));
    assert!(fb_msg.contains("RAW_NODE_RESULT_SAFE"));
}

#[tokio::test]
async fn secrets_encryption_at_rest_and_rotation() {
    use agenct_hub::secrets::{
        reencrypt_db, SecretCipher, SCOPE_GITLAB, SCOPE_IM, SCOPE_USER_MODELS,
    };

    let mock_llm = Router::new().route(
        "/v1/chat/completions",
        post(|headers: axum::http::HeaderMap| async move {
            assert_eq!(
                headers
                    .get(axum::http::header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok()),
                Some("Bearer sk-secret-brain-key-9876")
            );
            Json(json!({
                "choices": [{"message": {"content": "im webhook decrypted ok"}}]
            }))
        }),
    );
    let llm_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let llm_addr = llm_listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(llm_listener, mock_llm).await.unwrap();
    });

    let mut cfg = test_config("https://git.example.com");
    cfg.allow_private_outbound = true;
    cfg.master_key = Some("test-master-key-0123456789abcdef0123456789".into());
    let hub = Hub::open(cfg).unwrap();
    let router = app(hub.clone());

    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_c, _) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;

    let (st, m_res) = req_json(
        &router,
        "PUT",
        "/api/settings/models",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "brain": {
                "url": format!("http://{llm_addr}/v1/chat/completions"),
                "model": "brain-v1",
                "api_key": "sk-secret-brain-key-9876",
                "supports_images": false
            },
            "vision": {
                "url": format!("http://{llm_addr}/v1/chat/completions"),
                "model": "vision-v1",
                "api_key": "sk-secret-vision-key-4321"
            }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{m_res}");
    assert_eq!(m_res["brain"]["key_preview"], "****9876");

    let (st, gl_res) = req_json(
        &router,
        "PUT",
        "/api/integrations/gitlab",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "base_url": "https://git.example.com",
            "username": "alice",
            "token": "glpat-super-secret-token-7777"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{gl_res}");

    let (st, im_res) = req_json(
        &router,
        "PUT",
        "/api/integrations/im",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "provider": "feishu",
            "webhook_url": "https://open.feishu.cn/open-apis/bot/v2/hook/secret-hook-xyz",
            "rotate_inbound_token": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{im_res}");
    let inbound_token = im_res["inbound_token"].as_str().unwrap().to_owned();

    // Verify raw SQLite cells store AEAD ciphertext without leaking any plaintext secrets.
    let (raw_model, raw_gl, raw_im): (String, String, String) = hub
        .db
        .read(|conn| {
            let m: String = conn.query_row(
                "SELECT config_json FROM user_models WHERE user_id='u-alice'",
                [],
                |r| r.get(0),
            )?;
            let gl: String = conn.query_row(
                "SELECT config_json FROM integrations WHERE user_id='u-alice' AND kind='gitlab'",
                [],
                |r| r.get(0),
            )?;
            let im: String = conn.query_row(
                "SELECT config_json FROM integrations WHERE user_id='u-alice' AND kind='im'",
                [],
                |r| r.get(0),
            )?;
            Ok((m, gl, im))
        })
        .await
        .unwrap();
    for (label, cell, needle) in [
        ("user_models", &raw_model, "sk-secret-brain-key-9876"),
        (
            "user_models_vision",
            &raw_model,
            "sk-secret-vision-key-4321",
        ),
        ("gitlab", &raw_gl, "glpat-super-secret-token-7777"),
        ("im", &raw_im, "secret-hook-xyz"),
    ] {
        assert!(cell.starts_with("enc:v1:"), "{label} should be encrypted");
        assert!(!cell.contains(needle), "{label} leaked plaintext");
    }

    // Read APIs and inbound webhook decrypt transparently.
    let (st, m_get) = req_json(
        &router,
        "GET",
        "/api/settings/models",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(m_get["vision"]["key_preview"], "****4321");

    let (st, gl_get) = req_json(
        &router,
        "GET",
        "/api/integrations/gitlab",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(gl_get["token_preview"], "glpa****7777");

    let hook_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/im")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::CONTENT_TYPE, "application/json")
                .header("x-agenct-im-token", inbound_token)
                .body(Body::from(
                    json!({"text": "hello from im webhook"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hook_resp.status(), StatusCode::OK);

    // Seed a legacy plaintext row for Bob and verify transparent read + offline reencryption & rotation.
    hub.db
        .write(|conn| {
            conn.execute(
                "INSERT INTO user_models(user_id, config_json, updated_at) VALUES (?1, ?2, 1000)",
                rusqlite::params![
                    "u-bob",
                    json!({
                        "brain": {
                            "url": "https://llm.local/v1/chat/completions",
                            "api_key": "legacy-plain-key-12345",
                            "model": "legacy-m",
                            "supports_images": false
                        }
                    })
                    .to_string()
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    let (st, bob_get) = req_json(
        &router,
        "GET",
        "/api/settings/models",
        Some(&bob_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(bob_get["brain"]["key_preview"], "****2345");

    let migrated = hub
        .db
        .write({
            let current = hub.secrets.clone();
            move |conn| reencrypt_db(conn, &current, None)
        })
        .await
        .unwrap();
    assert_eq!(migrated, 1, "only legacy plaintext row should be rewritten");

    let rotated_key =
        SecretCipher::from_raw(Some("rotated-master-key-0123456789abcdef0123")).unwrap();
    let rotated = hub
        .db
        .write({
            let prev = hub.secrets.clone();
            let next = rotated_key.clone();
            move |conn| reencrypt_db(conn, &next, Some(&prev))
        })
        .await
        .unwrap();
    assert_eq!(rotated, 4, "all 4 encrypted rows should rotate to new key");

    // Re-running rotation is idempotent.
    let rotated_again = hub
        .db
        .write({
            let prev = hub.secrets.clone();
            let next = rotated_key.clone();
            move |conn| reencrypt_db(conn, &next, Some(&prev))
        })
        .await
        .unwrap();
    assert_eq!(rotated_again, 0);

    let bob_raw: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT config_json FROM user_models WHERE user_id='u-bob'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(bob_raw.starts_with("enc:v1:"));
    assert!(!bob_raw.contains("legacy-plain-key-12345"));
    assert!(hub
        .secrets
        .decrypt(SCOPE_USER_MODELS, "u-bob", &bob_raw)
        .is_err());
    assert!(rotated_key
        .decrypt(SCOPE_USER_MODELS, "u-bob", &bob_raw)
        .unwrap()
        .contains("legacy-plain-key-12345"));
    // Per-user AAD binding prevents cross-user decryption even with the active key.
    assert!(rotated_key
        .decrypt(SCOPE_USER_MODELS, "u-alice", &bob_raw)
        .is_err());
    assert!(rotated_key
        .decrypt(SCOPE_GITLAB, "u-bob", &bob_raw)
        .is_err());
    assert!(rotated_key.decrypt(SCOPE_IM, "u-bob", &bob_raw).is_err());
}

#[tokio::test]
async fn auth_user_extractor_enforces_active_and_reuses_guard() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let router = app(hub.clone());

    let (st, _) = req_json(&router, "GET", "/api/bootstrap", None, None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);

    let (bob_c, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;

    let (st, _) = req_json(&router, "GET", "/api/bootstrap", Some(&bob_c), None, None).await;
    assert_eq!(st, StatusCode::OK);

    // Invalid CSRF fails at guard with 403 before entering the handler.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/ledgers",
        Some(&bob_c),
        Some("wrong-csrf"),
        Some(json!({"title": "blocked"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/ledgers",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"title": "allowed"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Simulate an account disabled in DB while an existing session row remains:
    // both GET fallback and POST guard-extension paths in AuthUser must reject with 401.
    hub.db
        .write(|conn| {
            conn.execute("UPDATE users SET status='disabled' WHERE id='u-bob'", [])?;
            Ok(())
        })
        .await
        .unwrap();

    let (st, body) = req_json(&router, "GET", "/api/bootstrap", Some(&bob_c), None, None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "account disabled");

    let (st, body) = req_json(
        &router,
        "POST",
        "/api/ledgers",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"title": "disabled mutation"})),
    )
    .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "account disabled");
}

#[tokio::test]
async fn multi_engine_node_config_routing_and_session_isolation() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, multi_created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "id": "pc-multi",
            "name": "PC Multi",
            "kind": "pc",
            "default_agent": "opencode",
            "allowed_agents": ["claude", "codex", "opencode"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{multi_created}");
    assert!(multi_created["install_unix"]
        .as_str()
        .unwrap()
        .contains("--agent opencode --agents claude,codex,opencode"));
    let multi_tok = multi_created["token"].as_str().unwrap().to_owned();

    let (st, oc_created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "id": "pc-only-oc",
            "name": "PC Only OC",
            "kind": "pc"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{oc_created}");
    assert!(oc_created["install_unix"]
        .as_str()
        .unwrap()
        .contains("--agents auto"));
    let oc_tok = oc_created["token"].as_str().unwrap().to_owned();

    let poll_work = |tok: String, env_hdr: String| {
        let r = router.clone();
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .uri("/node/v1/work")
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                        .header("x-agenct-node-protocol", "2")
                        .header("x-agenct-node-kind", "pc")
                        .header("x-agenct-node-env", env_hdr)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
        }
    };

    // Nodes advertise installed engines; pc-multi filters 4 detected down to 3 Hub-allowed.
    let w1 = poll_work(
        multi_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode".into(),
    )
    .await;
    assert!(w1["job"].is_null());
    let w2 = poll_work(
        oc_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:opencode".into(),
    )
    .await;
    assert!(w2["job"].is_null());

    let (st, nodes_val) = req_json(&router, "GET", "/api/nodes", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let nodes = nodes_val.as_array().unwrap();
    let multi = nodes.iter().find(|n| n["id"] == "pc-multi").unwrap();
    assert_eq!(
        multi["detected_agents"],
        json!(["claude", "codex", "agy", "opencode"])
    );
    assert_eq!(
        multi["effective_agents"],
        json!(["claude", "codex", "opencode"])
    );
    assert_eq!(multi["effective_default_agent"], "opencode");

    // Reconfigure pc-multi from Web API: switch default engine to claude and narrow allowlist.
    let (st, upd) = req_json(
        &router,
        "PUT",
        "/api/nodes/pc-multi",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "default_agent": "claude",
            "allowed_agents": ["claude", "opencode"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{upd}");
    assert_eq!(upd["effective_default_agent"], "claude");
    assert_eq!(upd["effective_agents"], json!(["claude", "opencode"]));

    // Explicitly request claude -> must route to pc-multi (not pc-only-oc) and deliver agent="claude".
    let (st, chat1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --agent claude fix auth"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chat1}");
    assert_eq!(chat1["tasks"][0]["node_id"], "pc-multi");
    assert_eq!(chat1["tasks"][0]["agent"], "claude");
    let thr_id = chat1["thread_id"].as_str().unwrap().to_owned();

    let job1 = poll_work(
        multi_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode".into(),
    )
    .await;
    assert_eq!(job1["job"]["agent"], "claude");
    assert!(job1["job"]["agent_session_id"].is_null());

    let submit_job = |tok: String, job: &Value, sid: &str, agent: &str| {
        let r = router.clone();
        let tid = job["task_id"].as_str().unwrap().to_owned();
        let rev = job["task_revision"].as_i64().unwrap();
        let lease = job["lease_token"].as_str().unwrap().to_owned();
        let sid = sid.to_owned();
        let agent = agent.to_owned();
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/node/v1/tasks/{tid}/submit"))
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::AUTHORIZATION, format!("Bearer {tok}"))
                        .header("x-agenct-node-protocol", "2")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({
                                "lease_token": lease,
                                "request_id": format!("req-{tid}-{sid}"),
                                "expected_revision": rev,
                                "status": "completed",
                                "content": format!("ok from {agent}"),
                                "agent": agent,
                                "agent_session_id": sid,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    };
    submit_job(multi_tok.clone(), &job1["job"], "cl_sid_001", "claude").await;

    // Follow-up in the same thread using the same engine (`claude`) resumes `cl_sid_001`.
    let (tid2, _) = hub
        .db
        .write({
            let thr = thr_id.clone();
            move |conn| {
                agenct_hub::nodes::create_task_and_enqueue_with_options_sync(
                    conn,
                    "u-alice",
                    "u-alice",
                    "pc-multi",
                    "same-engine claude resume",
                    "continue with claude",
                    "manual",
                    agenct_hub::nodes::EnqueueOptions {
                        thread_id: Some(&thr),
                        on_complete: "",
                        callback_depth: 0,
                        agent: "claude",
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let cl_poll = poll_work(
        multi_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode".into(),
    )
    .await;
    assert_eq!(cl_poll["job"]["task_id"], tid2);
    assert_eq!(cl_poll["job"]["agent"], "claude");
    assert_eq!(cl_poll["job"]["agent_session_id"], "cl_sid_001");
    submit_job(multi_tok.clone(), &cl_poll["job"], "cl_sid_001", "claude").await;

    // Switching engines on the same thread (`claude` -> `opencode`) must NOT leak `cl_sid_001`.
    let (tid_oc, _) = hub
        .db
        .write({
            let thr = thr_id.clone();
            move |conn| {
                agenct_hub::nodes::create_task_and_enqueue_with_options_sync(
                    conn,
                    "u-alice",
                    "u-alice",
                    "pc-multi",
                    "switch engine to opencode",
                    "continue with opencode",
                    "manual",
                    agenct_hub::nodes::EnqueueOptions {
                        thread_id: Some(&thr),
                        on_complete: "",
                        callback_depth: 0,
                        agent: "opencode",
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let oc_job_poll = poll_work(
        multi_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode".into(),
    )
    .await;
    assert_eq!(oc_job_poll["job"]["task_id"], tid_oc);
    assert_eq!(oc_job_poll["job"]["agent"], "opencode");
    assert!(
        oc_job_poll["job"]["agent_session_id"].is_null(),
        "switching engine must start a fresh session instead of passing claude session_id"
    );
    submit_job(
        multi_tok.clone(),
        &oc_job_poll["job"],
        "oc_sid_002",
        "opencode",
    )
    .await;

    // Subsequent opencode turn on the same thread resumes `oc_sid_002`.
    let (tid_oc2, _) = hub
        .db
        .write({
            let thr = thr_id.clone();
            move |conn| {
                agenct_hub::nodes::create_task_and_enqueue_with_options_sync(
                    conn,
                    "u-alice",
                    "u-alice",
                    "pc-multi",
                    "resume opencode session",
                    "next opencode turn",
                    "manual",
                    agenct_hub::nodes::EnqueueOptions {
                        thread_id: Some(&thr),
                        on_complete: "",
                        callback_depth: 0,
                        agent: "opencode",
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();
    let oc_poll2 = poll_work(
        multi_tok.clone(),
        "kind:pc | os:linux | agent:opencode | agents:claude,codex,agy,opencode".into(),
    )
    .await;
    assert_eq!(oc_poll2["job"]["task_id"], tid_oc2);
    assert_eq!(oc_poll2["job"]["agent"], "opencode");
    assert_eq!(oc_poll2["job"]["agent_session_id"], "oc_sid_002");
    submit_job(
        multi_tok.clone(),
        &oc_poll2["job"],
        "oc_sid_002",
        "opencode",
    )
    .await;

    // Dispatching disabled engine `agy` must fail cleanly without queuing a broken job.
    let (st, rejected) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --agent agy unsupported check"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(rejected["tasks"].as_array().unwrap().is_empty());
    assert!(rejected["reply"].as_str().unwrap().contains("agy"));
}

#[tokio::test]
async fn chat_managed_roles_model_dispatch_and_session_isolation() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "name": "PC Multi",
            "kind": "pc",
            "allowed_agents": ["claude", "codex", "opencode"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{created}");
    let tok = created["token"].as_str().unwrap().to_owned();

    let poll_work = || {
        let r = router.clone();
        let t = tok.clone();
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .uri("/node/v1/work")
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::AUTHORIZATION, format!("Bearer {t}"))
                        .header("x-agenct-node-protocol", "2")
                        .header("x-agenct-node-kind", "pc")
                        .header(
                            "x-agenct-node-env",
                            "kind:pc | os:linux | agent:codex | agents:claude,codex,opencode",
                        )
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            serde_json::from_slice::<Value>(&resp.into_body().collect().await.unwrap().to_bytes())
                .unwrap()
        }
    };
    let submit_job = |job: &Value, sid: &str, agent: &str| {
        let r = router.clone();
        let t = tok.clone();
        let tid = job["task_id"].as_str().unwrap().to_owned();
        let rev = job["task_revision"].as_i64().unwrap();
        let lease = job["lease_token"].as_str().unwrap().to_owned();
        let sid = sid.to_owned();
        let agent = agent.to_owned();
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/node/v1/tasks/{tid}/submit"))
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::AUTHORIZATION, format!("Bearer {t}"))
                        .header("x-agenct-node-protocol", "2")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            json!({
                                "lease_token": lease,
                                "request_id": format!("req-{tid}-{sid}"),
                                "expected_revision": rev,
                                "status": "completed",
                                "content": format!("ok from {agent}"),
                                "agent": agent,
                                "agent_session_id": sid,
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    };

    assert!(poll_work().await["job"].is_null());

    // Create planner + executor roles purely through chat.
    let (st, r1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "message": "/role planner agent=claude model=opus-5.5 effort=high sandbox=readonly desc=\"Architecture planner\" prompt=\"Analyze only; never modify files\""
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r1}");
    let (st, r2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "message": "/role executor agent=codex model=gpt-6.2-sol effort=medium sandbox=workspace-write"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{r2}");

    let (st, roles_val) = req_json(&router, "GET", "/api/roles", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let roles = roles_val.as_array().unwrap();
    assert_eq!(roles.len(), 2);
    assert_eq!(roles[0]["name"], "executor");
    assert_eq!(roles[1]["name"], "planner");

    // Partial update via chat swaps model while preserving engine/effort/sandbox/prompt.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/role planner model=claude-opus-4-6"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let planner = boot["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "planner")
        .unwrap();
    assert_eq!(planner["agent"], "claude");
    assert_eq!(planner["model"], "claude-opus-4-6");
    assert_eq!(planner["effort"], "high");
    assert_eq!(planner["sandbox"], "readonly");
    assert_eq!(planner["prompt"], "Analyze only; never modify files");

    // Security guards: disallow unsafe-full sandbox escalation and unsafe model flags.
    let (st, bad_sb) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/role hacker agent=claude sandbox=unsafe-full"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(bad_sb["reply"].as_str().unwrap().contains("readonly"));
    let (st, bad_model) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/role planner model=\"-bad flag\""})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(bad_model["reply"]
        .as_str()
        .unwrap()
        .contains("模型名称无效"));

    let (st, roles_after_bad) =
        req_json(&router, "GET", "/api/roles", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(roles_after_bad.as_array().unwrap().len(), 2);
    assert_eq!(
        roles_after_bad
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == "planner")
            .unwrap()["model"],
        "claude-opus-4-6"
    );

    // Dispatch with --role planner: attaches role prompt, model, effort, readonly sandbox.
    let (st, c1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --role planner inspect repo layout"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c1}");
    assert_eq!(c1["tasks"][0]["role"], "planner");
    assert_eq!(c1["tasks"][0]["agent"], "claude");
    assert_eq!(c1["tasks"][0]["model"], "claude-opus-4-6");
    let thr_id = c1["thread_id"].as_str().unwrap().to_owned();

    let w1 = poll_work().await;
    assert_eq!(w1["job"]["role"], "planner");
    assert_eq!(w1["job"]["agent"], "claude");
    assert_eq!(w1["job"]["model"], "claude-opus-4-6");
    assert_eq!(w1["job"]["effort"], "high");
    assert_eq!(w1["job"]["sandbox"], "readonly");
    assert!(w1["job"]["instruction"]
        .as_str()
        .unwrap()
        .contains("Analyze only; never modify files"));
    assert!(w1["job"]["instruction"]
        .as_str()
        .unwrap()
        .contains("inspect repo layout"));
    submit_job(&w1["job"], "plan_sid_1", "claude").await;

    // Follow-up in same thread with same role & model resumes `plan_sid_1`.
    let (st, c2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("/node --thread {thr_id} --role planner inspect tests")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c2}");
    let w2 = poll_work().await;
    assert_eq!(w2["job"]["agent_session_id"], "plan_sid_1");
    submit_job(&w2["job"], "plan_sid_1", "claude").await;

    // Per-call model override in same thread/role isolates session (agent_session_id must be null).
    let (st, c3) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "message": format!("/node --thread {thr_id} --role planner --model sonnet-4 --effort low quick check")
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c3}");
    let w3 = poll_work().await;
    assert_eq!(w3["job"]["role"], "planner");
    assert_eq!(w3["job"]["model"], "sonnet-4");
    assert_eq!(w3["job"]["effort"], "low");
    assert_eq!(w3["job"]["sandbox"], "readonly");
    assert!(w3["job"]["agent_session_id"].is_null());
    submit_job(&w3["job"], "plan_sid_sonnet", "claude").await;

    // Switching role in same thread (`planner` -> `executor`) also isolates session.
    let (st, c4) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("/node --thread {thr_id} --role executor implement fix")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{c4}");
    let w4 = poll_work().await;
    assert_eq!(w4["job"]["role"], "executor");
    assert_eq!(w4["job"]["agent"], "codex");
    assert_eq!(w4["job"]["model"], "gpt-6.2-sol");
    assert_eq!(w4["job"]["effort"], "medium");
    assert_eq!(w4["job"]["sandbox"], "workspace-write");
    assert!(w4["job"]["agent_session_id"].is_null());
    submit_job(&w4["job"], "exec_sid_1", "codex").await;

    // Repeating executor in the same thread resumes `exec_sid_1`.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": format!("/node --thread {thr_id} --role executor run cargo test")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let w5 = poll_work().await;
    assert_eq!(w5["job"]["agent_session_id"], "exec_sid_1");
    submit_job(&w5["job"], "exec_sid_1", "codex").await;

    // Receipt formatting shows role/engine/model cleanly in chat history.
    let (st, boot_after) =
        req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let contents: Vec<&str> = boot_after["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect();
    assert!(contents
        .iter()
        .any(|c| c.contains("PC Multi (planner / claude / claude-opus-4-6)")));
    assert!(contents
        .iter()
        .any(|c| c.contains("PC Multi (executor / codex / gpt-6.2-sol)")));

    // Delete via HTTP requires CSRF, and delete via chat works seamlessly.
    let (st, _) = req_json(
        &router,
        "DELETE",
        "/api/roles/executor",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    let (st, del_http) = req_json(
        &router,
        "DELETE",
        "/api/roles/executor",
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{del_http}");

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/role delete planner"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, final_roles) = req_json(&router, "GET", "/api/roles", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert!(final_roles.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn verbose_node_submit_auto_digests_without_on_complete_while_short_posts_directly() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-digest", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    // Online handshake so node is eligible for /node dispatch.
    let poll_empty = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header("x-agenct-node-kind", "pc")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(poll_empty.status(), StatusCode::OK);

    // 1. Verbose report without `on_complete` must trigger automatic Brain digest while keeping full raw result in drawer.
    let (st, chat1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node inspect service logs"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let tid1 = chat1["tasks"][0]["task_id"].as_str().unwrap().to_owned();

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
    let job1: Value =
        serde_json::from_slice(&poll1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let lease1 = job1["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev1 = job1["job"]["task_revision"].as_i64().unwrap();

    let raw_verbose = "## 完整日志排查结论\n服务已恢复健康。\n- 检查项 1：端口监听正常\n- 检查项 2：数据库连接正常\n- 检查项 3：队列积压已清空\nRAW_VERBOSE_MARKER_DO_NOT_DUMP";
    let png_b64 = STANDARD.encode([0x89u8, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4]);
    let mut rx = hub.events.subscribe("u-alice");
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
                        "request_id": "sub-digest-verbose",
                        "expected_revision": rev1,
                        "status": "completed",
                        "content": raw_verbose,
                        "attachments": [{"name": "screen.png", "mime_type": "image/png", "data_base64": png_b64}]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub1.status(), StatusCode::OK);

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let ev = rx.recv().await.unwrap();
            if ev.kind == "message" && ev.data.contains("\"callback\":true") {
                break;
            }
        }
    })
    .await
    .expect("auto-digest should emit completion message event");

    let (msgs, saved_result, digest_audits): (Vec<String>, Option<String>, i64) = hub
        .db
        .read({
            let tid = tid1.clone();
            move |conn| {
                let mut ms = conn
                    .prepare("SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id")?;
                let msgs = ms
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                let saved_result: Option<String> =
                    conn.query_row("SELECT result FROM tasks WHERE id=?1", [&tid], |r| r.get(0))?;
                let digest_audits: i64 = conn.query_row(
                    "SELECT count(*) FROM audit_logs WHERE user_id='u-alice' AND action='task_digest' AND status='ok'",
                    [],
                    |r| r.get(0),
                )?;
                Ok((msgs, saved_result, digest_audits))
            }
        })
        .await
        .unwrap();

    assert!(saved_result
        .as_deref()
        .is_some_and(|r| r.contains("RAW_VERBOSE_MARKER_DO_NOT_DUMP")));
    assert_eq!(digest_audits, 1);
    let last_msg = msgs.last().unwrap();
    assert!(last_msg.contains("**[节点 pc-digest"));
    assert!(last_msg.contains("已提炼节点结论：## 完整日志排查结论"));
    assert!(last_msg.contains("/api/attachments/att_"));
    assert!(!last_msg.contains("RAW_VERBOSE_MARKER_DO_NOT_DUMP"));

    // 2. Concise 1-line node output skips auto-digest and posts immediately.
    let (st, chat2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node check uptime"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let tid2 = chat2["tasks"][0]["task_id"].as_str().unwrap().to_owned();
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
    let job2: Value =
        serde_json::from_slice(&poll2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let lease2 = job2["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev2 = job2["job"]["task_revision"].as_i64().unwrap();
    let sub2 = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/node/v1/tasks/{tid2}/submit"))
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    json!({
                        "lease_token": lease2,
                        "request_id": "sub-short-direct",
                        "expected_revision": rev2,
                        "status": "completed",
                        "content": "uptime: 14 days, load 0.12"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub2.status(), StatusCode::OK);

    let (latest_msg, audits_after): (String, i64) = hub
        .db
        .read(|conn| {
            let msg: String = conn.query_row(
                "SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?;
            let cnt: i64 = conn.query_row(
                "SELECT count(*) FROM audit_logs WHERE user_id='u-alice' AND action='task_digest'",
                [],
                |r| r.get(0),
            )?;
            Ok((msg, cnt))
        })
        .await
        .unwrap();
    assert!(latest_msg.contains("uptime: 14 days, load 0.12"));
    assert_eq!(audits_after, 1);
}
