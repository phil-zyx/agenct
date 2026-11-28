use agenct_hub::{
    app,
    config::{Config, GitLabOAuthConfig, LlmConfig},
    Hub,
};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    response::Response,
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

async fn req_json_raw(
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

async fn req_json(
    router: &Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    csrf: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    if !(method == "POST" && uri == "/api/chat" && cookie.is_some() && csrf.is_some()) {
        return req_json_raw(router, method, uri, cookie, csrf, body).await;
    }
    let (_, pre_boot) = req_json_raw(router, "GET", "/api/bootstrap", cookie, None, None).await;
    let prev_task_ids: HashSet<String> = pre_boot["tasks"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();

    let (st, accepted) = req_json_raw(router, method, uri, cookie, csrf, body).await;
    if st != StatusCode::ACCEPTED {
        return (st, accepted);
    }
    let msg_id = accepted["message_id"]
        .as_i64()
        .expect("202 /api/chat must return message_id");

    for _ in 0..400 {
        let (bst, boot) = req_json_raw(router, "GET", "/api/bootstrap", cookie, None, None).await;
        if bst != StatusCode::OK {
            return (bst, boot);
        }
        let msgs = boot["messages"].as_array().cloned().unwrap_or_default();
        if let Some(user_msg) = msgs.iter().find(|m| m["id"].as_i64() == Some(msg_id)) {
            let m_status = user_msg["status"].as_str().unwrap_or("");
            if m_status == "failed" {
                return (
                    StatusCode::BAD_GATEWAY,
                    json!({"error": "chat turn failed", "message_id": msg_id}),
                );
            }
            if m_status == "complete" {
                let reply = msgs
                    .iter()
                    .find(|m| {
                        m["role"].as_str() == Some("assistant")
                            && m["reply_to"].as_i64() == Some(msg_id)
                    })
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_owned();
                let new_tasks: Vec<Value> = boot["tasks"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter(|t| {
                                t["id"]
                                    .as_str()
                                    .is_some_and(|id| !prev_task_ids.contains(id))
                            })
                            .map(|t| {
                                let mut obj = t.clone();
                                if let Some(map) = obj.as_object_mut() {
                                    if let Some(id) = map.get("id").cloned() {
                                        map.insert("task_id".into(), id);
                                    }
                                    let mode = if map.get("status").and_then(Value::as_str)
                                        == Some("pending")
                                    {
                                        "pending_manual"
                                    } else {
                                        "auto_queued"
                                    };
                                    map.insert("mode".into(), json!(mode));
                                }
                                obj
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                return (
                    StatusCode::OK,
                    json!({
                        "reply": reply,
                        "thread_id": user_msg["thread_id"],
                        "tasks": new_tasks,
                        "message_id": msg_id,
                    }),
                );
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for async /api/chat message_id={msg_id} to settle");
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
    let (job_status, task_status): (String, String) = hub
        .db
        .read(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT status FROM node_jobs WHERE task_id = ?1",
                    [&auto_tid],
                    |r| r.get(0),
                )?,
                conn.query_row("SELECT status FROM tasks WHERE id = ?1", [&auto_tid], |r| {
                    r.get(0)
                })?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(job_status, "failed");
    assert_eq!(task_status, "failed");

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
async fn native_tool_calling_and_streaming_work() {
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
        Some(json!({"message": "/node_cb 验收测试结果并更新记忆 :: run tests"})),
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
    assert_eq!(on_cb, "验收测试结果并更新记忆");
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

    let (msgs, memory_md, audit_cnt, saved_result): (Vec<String>, String, i64, Option<String>) =
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
                    let memory_md: String = conn.query_row(
                        "SELECT memory_md FROM agent_profiles WHERE user_id='u-alice'",
                        [],
                        |r| r.get(0),
                    )?;
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
                    Ok((msgs, memory_md, audit_cnt, saved_result))
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
    assert!(memory_md.contains("自动跟进事项：节点已返回结果并完成验收"));
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
        None,
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
        None,
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
        None,
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
            "wecom": {
                "enabled": false,
                "bot_id": "wecom-bot-alice",
                "secret": "wecom-sec-secret-111"
            },
            "dingtalk": {
                "enabled": false,
                "client_id": "dd-client-alice",
                "client_secret": "dd-sec-secret-222"
            },
            "provider": "feishu",
            "webhook_url": "https://open.feishu.cn/open-apis/bot/v2/hook/secret-hook-xyz",
            "webhook_secret": "wh-sec-333",
            "issue_bind_code": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{im_res}");
    assert!(im_res["bind_code"].as_str().is_some_and(|c| c.len() == 6));

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
        ("im_hook", &raw_im, "secret-hook-xyz"),
        ("im_wecom", &raw_im, "wecom-sec-secret-111"),
        ("im_dd", &raw_im, "dd-sec-secret-222"),
        ("im_whsec", &raw_im, "wh-sec-333"),
    ] {
        assert!(cell.starts_with("enc:v1:"), "{label} should be encrypted");
        assert!(!cell.contains(needle), "{label} leaked plaintext");
    }

    // Read APIs decrypt transparently and mask stored secrets.
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

    let (st, im_get) = req_json(
        &router,
        "GET",
        "/api/integrations/im",
        Some(&alice_c),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(im_get["wecom"]["secret_preview"], "****-111");
    assert_eq!(im_get["dingtalk"]["secret_preview"], "****-222");
    assert_eq!(im_get["webhook_secret_configured"], true);

    // Obsolete unstandardised /webhooks/im route has been removed (ServeDir fallback rejects POST with 405).
    let hook_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/im")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(json!({"text": "legacy"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(
            hook_resp.status(),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
        ),
        "unexpected status for removed /webhooks/im: {}",
        hook_resp.status()
    );

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
        "PUT",
        "/api/brain",
        Some(&bob_c),
        Some("wrong-csrf"),
        Some(json!({"memory_md": "blocked"})),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/brain",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"memory_md": "allowed"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // Simulate an account disabled in DB while an existing session row remains:
    // both GET fallback and mutation guard-extension paths in AuthUser must reject with 401.
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
        "PUT",
        "/api/brain",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"memory_md": "disabled mutation"})),
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

#[tokio::test]
async fn orphaned_running_tasks_fail_on_lease_expiry_rotation_and_node_removal() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, node) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-reddot", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_id = node["node"]["id"].as_str().unwrap().to_owned();
    let mut node_token = node["token"].as_str().unwrap().to_owned();

    // Online handshake so node is eligible for /node dispatch.
    let handshake = router
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
    assert_eq!(handshake.status(), StatusCode::OK);

    // Helper to enqueue + claim a task on pc-reddot.
    let dispatch_and_claim = |title: &str, token: &str| {
        let router = router.clone();
        let cookie = cookie.clone();
        let csrf = csrf.clone();
        let title = title.to_owned();
        let token = token.to_owned();
        async move {
            let (st, chat) = req_json(
                &router,
                "POST",
                "/api/chat",
                Some(&cookie),
                Some(&csrf),
                Some(json!({"message": format!("/node {title}")})),
            )
            .await;
            assert_eq!(st, StatusCode::OK);
            let tid = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

            let poll = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/node/v1/work")
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .header("x-agenct-node-protocol", "2")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(poll.status(), StatusCode::OK);
            let body: Value =
                serde_json::from_slice(&poll.into_body().collect().await.unwrap().to_bytes())
                    .unwrap();
            assert_eq!(body["job"]["task_id"].as_str(), Some(tid.as_str()));
            tid
        }
    };

    // 1. Lease expiry + global background reap fails both node_job and parent task.
    let tid_exp = dispatch_and_claim("lease expiry task", &node_token).await;
    let tid_exp_q = tid_exp.clone();
    hub.db
        .write(move |conn| {
            conn.execute(
                "UPDATE node_jobs SET lease_until = 1 WHERE task_id = ?1",
                [&tid_exp_q],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    agenct_hub::nodes::reap_expired_leases(&hub).await;

    let (j1, t1, r1): (String, String, String) = hub
        .db
        .read(move |conn| {
            Ok((
                conn.query_row(
                    "SELECT status FROM node_jobs WHERE task_id = ?1",
                    [&tid_exp],
                    |r| r.get(0),
                )?,
                conn.query_row("SELECT status FROM tasks WHERE id = ?1", [&tid_exp], |r| {
                    r.get(0)
                })?,
                conn.query_row(
                    "SELECT COALESCE(result, '') FROM tasks WHERE id = ?1",
                    [&tid_exp],
                    |r| r.get(0),
                )?,
            ))
        })
        .await
        .unwrap();
    assert_eq!(j1, "failed");
    assert_eq!(t1, "failed");
    assert!(!r1.is_empty());

    // 2. Token rotation while a task is running also fails the parent task.
    let tid_rot = dispatch_and_claim("token rotate task", &node_token).await;
    let (st, rot_resp) = req_json(
        &router,
        "POST",
        &format!("/api/nodes/{node_id}/token"),
        Some(&cookie),
        Some(&csrf),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    node_token = rot_resp["token"].as_str().unwrap().to_owned();

    let t2: String = hub
        .db
        .read(move |conn| {
            Ok(
                conn.query_row("SELECT status FROM tasks WHERE id = ?1", [&tid_rot], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(t2, "failed");

    // 3. Removing a node while a task is running cascades node_jobs and fails the parent task.
    let tid_rm = dispatch_and_claim("node remove task", &node_token).await;
    let (st, _) = req_json(
        &router,
        "DELETE",
        &format!("/api/nodes/{node_id}"),
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let t3: String = hub
        .db
        .read(move |conn| {
            Ok(
                conn.query_row("SELECT status FROM tasks WHERE id = ?1", [&tid_rm], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(t3, "failed");

    // 4. Bootstrap active task list has zero pending/running tasks left (red badge cleared).
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let active_count = boot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| matches!(t["status"].as_str(), Some("pending" | "running")))
        .count();
    assert_eq!(active_count, 0);
}

#[tokio::test]
async fn chat_manage_node_switches_default_agent_and_persists_for_dispatch() {
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
            "id": "homelab",
            "name": "HomeLab",
            "kind": "pc",
            "default_agent": "codex",
            "allowed_agents": ["codex", "agy", "opencode"]
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    // Online handshake so node is eligible for /node dispatch.
    let handshake = router
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
    assert_eq!(handshake.status(), StatusCode::OK);

    // 1. Chat triggers manage_node to switch homelab default_agent -> agy.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/nodecfg homelab default=agy"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, nodes_after_agy) =
        req_json(&router, "GET", "/api/nodes", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nodes_after_agy[0]["default_agent"], "agy");
    assert_eq!(nodes_after_agy[0]["effective_default_agent"], "agy");

    // 2. Subsequent /node dispatch without explicit agent inherits the persisted default ("agy").
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node inspect status"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

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
    assert_eq!(poll.status(), StatusCode::OK);
    let claimed: Value =
        serde_json::from_slice(&poll.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(claimed["job"]["agent"], "agy");

    // 3. Omitting node_id auto-resolves single/sole-online node -> switches to opencode.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/nodecfg default=opencode"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, nodes_after_oc) =
        req_json(&router, "GET", "/api/nodes", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nodes_after_oc[0]["default_agent"], "opencode");

    // 4. Invalid engine name fails gracefully without corrupting persisted default_agent.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/nodecfg homelab default=invalid_engine"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, nodes_after_err) =
        req_json(&router, "GET", "/api/nodes", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(nodes_after_err[0]["default_agent"], "opencode");
}

#[tokio::test]
async fn node_submit_accepts_backend_field_and_records_audit() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-backend", "kind": "pc"})),
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
        Some(json!({"message": "/node check backend receipt"})),
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

    // Unknown fields must still return 422 due to deny_unknown_fields.
    let unknown_resp = router
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
                        "request_id": "sub-unknown",
                        "expected_revision": rev,
                        "status": "completed",
                        "content": "ok",
                        "unexpected_field": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unknown_resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Valid submit with backend field succeeds (200 OK) and records backend in audit_logs.
    let ok_resp = router
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
                        "request_id": "sub-backend-1",
                        "expected_revision": rev,
                        "status": "completed",
                        "content": "Done via tmux.",
                        "agent": "codex",
                        "backend": "tmux"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ok_resp.status(), StatusCode::OK);

    let audit_title: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT title FROM audit_logs WHERE action = 'node_submit' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(
        audit_title.contains("pc-backend [tmux]:"),
        "expected backend in audit title, got: {audit_title}"
    );
}

#[tokio::test]
async fn chat_pinned_thread_id_persists_discussion_and_rejects_foreign_thread() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_c, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"name": "pc-1", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 1. Alice starts a topic thread via /node dispatch.
    let (st, chat1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "/node 排查移动端输入框布局"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let thr_id = chat1["thread_id"].as_str().unwrap().to_owned();

    // 2. Pure follow-up discussion in the pinned thread stays attached to thr_id.
    let (st, chat2) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "为什么窄屏要用双行布局？", "thread_id": thr_id})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{chat2}");
    assert_eq!(chat2["thread_id"].as_str(), Some(thr_id.as_str()));

    let (msg_count, turns): (i64, i64) = hub
        .db
        .read({
            let tid = thr_id.clone();
            move |conn| {
                let cnt: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM messages WHERE user_id='u-alice' AND thread_id=?1",
                    [&tid],
                    |r| r.get(0),
                )?;
                let tc: i64 = conn.query_row(
                    "SELECT turn_count FROM threads WHERE id=?1 AND user_id='u-alice'",
                    [&tid],
                    |r| r.get(0),
                )?;
                Ok((cnt, tc))
            }
        })
        .await
        .unwrap();
    assert_eq!(msg_count, 4);
    assert_eq!(turns, 2);

    // 3. Cross-user pinned thread_id is rejected (400 BAD_REQUEST).
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"message": "偷窥别人的话题", "thread_id": thr_id})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn thread_update_rename_and_stale_archive_hygiene() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-1", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node 检查 composer 高度"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let thr_id = chat["thread_id"].as_str().unwrap().to_owned();

    // 1. Rename via PUT /api/threads/{id}.
    let (st, _) = req_json(
        &router,
        "PUT",
        &format!("/api/threads/{thr_id}"),
        Some(&cookie),
        Some(&csrf),
        Some(json!({"title": "移动端输入框修复"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 2. Empty update is rejected.
    let (st, _) = req_json(
        &router,
        "PUT",
        &format!("/api/threads/{thr_id}"),
        Some(&cookie),
        Some(&csrf),
        Some(json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // 3. Bootstrap reflects updated title and turn_count.
    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let thr_view = boot["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == thr_id)
        .unwrap();
    assert_eq!(thr_view["title"], "移动端输入框修复");
    assert_eq!(thr_view["turn_count"], 1);

    // 4. Stale (>48h) active threads are archived and >10m empty shells are deleted by archive_stale_threads().
    let now = agenct_hub::config::now_unix();
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO threads(id, user_id, title, status, summary, turn_count, updated_at)
                 VALUES ('thr_stale', 'u-alice', '陈年旧话题', 'active', 'old', 2, ?1),
                        ('thr_empty', 'u-alice', '空壳线程', 'active', '', 0, ?2)",
                rusqlite::params![now - 49 * 3600, now - 1200],
            )?;
            Ok(())
        })
        .await
        .unwrap();

    agenct_hub::brain::archive_stale_threads(&hub).await;

    let (stale_status, empty_exists, fresh_status): (String, i64, String) = hub
        .db
        .read(move |conn| {
            let st: String =
                conn.query_row("SELECT status FROM threads WHERE id='thr_stale'", [], |r| {
                    r.get(0)
                })?;
            let emp: i64 = conn.query_row(
                "SELECT COUNT(*) FROM threads WHERE id='thr_empty'",
                [],
                |r| r.get(0),
            )?;
            let fr: String =
                conn.query_row("SELECT status FROM threads WHERE id=?1", [&thr_id], |r| {
                    r.get(0)
                })?;
            Ok((st, emp, fr))
        })
        .await
        .unwrap();
    assert_eq!(stale_status, "archived");
    assert_eq!(empty_exists, 0);
    assert_eq!(fresh_status, "active");
}

#[tokio::test]
async fn cron_and_ephemeral_dispatch_do_not_create_noise_threads() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-1", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 1. One-off dispatch with thread_id="none" creates a task without creating any thread.
    let (st, chat1) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --thread none 检查节点进程快照"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(chat1["thread_id"].is_null());
    assert_eq!(chat1["tasks"].as_array().unwrap().len(), 1);
    assert!(chat1["tasks"][0]["thread_id"].is_null());

    // 2. Headless cron dispatch does not create threads or attach cron report messages to threads.
    let _cron_id = create_due_cron(
        &router,
        &hub,
        &cookie,
        &csrf,
        "/node 每日巡检 homelab 状态",
        "cloud",
    )
    .await;
    assert_eq!(
        agenct_hub::automation::run_due_crons(&hub).await.unwrap(),
        1
    );

    let (thr_cnt, cron_thr_msgs, total_tasks): (i64, i64, i64) = hub
        .db
        .read(|conn| {
            let tc: i64 = conn.query_row("SELECT COUNT(*) FROM threads", [], |r| r.get(0))?;
            let cm: i64 = conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE content LIKE '%[定时汇报]%' AND thread_id IS NOT NULL",
                [],
                |r| r.get(0),
            )?;
            let tk: i64 = conn.query_row("SELECT COUNT(*) FROM tasks WHERE thread_id IS NULL", [], |r| r.get(0))?;
            Ok((tc, cm, tk))
        })
        .await
        .unwrap();
    assert_eq!(
        thr_cnt, 0,
        "ephemeral dispatch and cron must not create threads"
    );
    assert_eq!(cron_thr_msgs, 0);
    assert_eq!(total_tasks, 2);
}

#[tokio::test]
async fn sandbox_blocked_task_and_one_shot_manual_elevation_enforce_boundaries() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_c, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"name": "pc-sb", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    // 1. LLM cannot self-escalate via sandbox="elevated".
    let (st, bad_sb) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "/node --sandbox elevated 尝试自我提权"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(bad_sb["tasks"].as_array().unwrap().is_empty());

    // 2. Dispatch a normal task; node polls WITHOUT elevated:1 advertised.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "/node --agent opencode 查进程快照"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    let work1 = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(
                    "x-agenct-node-env",
                    "kind:pc | os:linux | host:lab | agent:opencode | agents:opencode | sandbox:workspace-write",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(work1.status(), StatusCode::OK);
    let w1: Value =
        serde_json::from_slice(&work1.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(w1["job"]["authorization"], "manual");
    let lease1 = w1["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev1 = w1["job"]["task_revision"].as_i64().unwrap();

    // Submit failure with blocked_reason -> persisted on task and exposed via /api/bootstrap.
    let sub1 = router
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
                        "lease_token": lease1,
                        "request_id": "sub-blk-1",
                        "expected_revision": rev1,
                        "status": "failed",
                        "content": "! permission requested: external_directory (/proc/*); auto-rejecting",
                        "agent": "opencode",
                        "blocked_reason": "permission requested: external_directory (/proc/*); auto-rejecting"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub1.status(), StatusCode::OK);

    let (st, boot) = req_json(&router, "GET", "/api/bootstrap", Some(&alice_c), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let tv = boot["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == task_id)
        .unwrap();
    assert_eq!(
        tv["blocked_reason"],
        "permission requested: external_directory (/proc/*); auto-rejecting"
    );

    // 3. retry_elevated fails while node does not advertise elevated:1.
    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"action": "retry_elevated"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // Node heartbeats with elevated:1; retry_elevated succeeds once and clears blocked_reason.
    let work_poll_elev = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(
                    "x-agenct-node-env",
                    "kind:pc | os:linux | host:lab | agent:opencode | agents:opencode | sandbox:workspace-write | elevated:1",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(work_poll_elev.status(), StatusCode::OK);

    // Cross-user cannot elevate Alice's task.
    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"action": "retry_elevated"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let (st, elev_res) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"action": "retry_elevated"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{elev_res}");
    assert_eq!(elev_res["sandbox"], "elevated");

    let work2 = router
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
    assert_eq!(work2.status(), StatusCode::OK);
    let w2: Value =
        serde_json::from_slice(&work2.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(w2["job"]["sandbox"], "elevated");
    assert_eq!(w2["job"]["authorization"], "manual");
    let lease2 = w2["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev2 = w2["job"]["task_revision"].as_i64().unwrap();

    // Submit failure again, then normal assign_node must reset sandbox back to normal (None).
    let sub2 = router
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
                        "lease_token": lease2,
                        "request_id": "sub-blk-2",
                        "expected_revision": rev2,
                        "status": "failed",
                        "content": "still failed"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub2.status(), StatusCode::OK);

    let (st, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"action": "assign_node"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let work3 = router
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
    let w3: Value =
        serde_json::from_slice(&work3.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert!(w3["job"]["sandbox"].is_null());
    let lease3 = w3["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev3 = w3["job"]["task_revision"].as_i64().unwrap();

    // Running task cannot be elevated mid-flight.
    let (st_run, _) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"action": "retry_elevated"})),
    )
    .await;
    assert_eq!(st_run, StatusCode::BAD_REQUEST);

    // Completed task CAN be elevated on demand by owner (e.g., to finish skipped `git push`).
    let sub3 = router
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
                        "lease_token": lease3,
                        "request_id": "sub-done-3",
                        "expected_revision": rev3,
                        "status": "completed",
                        "content": "local commit done, push skipped due to sandbox"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub3.status(), StatusCode::OK);

    let (st_done_elev, done_elev_res) = req_json(
        &router,
        "POST",
        &format!("/api/tasks/{task_id}/action"),
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"action": "retry_elevated"})),
    )
    .await;
    assert_eq!(st_done_elev, StatusCode::OK, "{done_elev_res}");
    assert_eq!(done_elev_res["sandbox"], "elevated");

    let elev_audits: i64 = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE action = 'task_elevate' AND user_id = 'u-alice'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(elev_audits, 2);
}

#[tokio::test]
async fn im_and_chat_approve_code_elevates_blocked_task_once_without_llm() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    let mut cfg = test_config("https://git.example.com");
    cfg.allow_private_outbound = true;
    let hub = Hub::open(cfg).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (_bob_c, _bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    // Local mock WeCom AI Bot WebSocket server.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_addr = ws_listener.local_addr().unwrap();
    let (to_ws_tx, mut to_ws_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let (from_ws_tx, mut from_ws_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((stream, _)) = ws_listener.accept().await else {
            return;
        };
        let mut ws = accept_async(stream).await.unwrap();
        let sub_msg = ws.next().await.unwrap().unwrap();
        let sub_val: Value = serde_json::from_str(sub_msg.to_text().unwrap()).unwrap();
        assert_eq!(sub_val["cmd"], "aibot_subscribe");
        ws.send(Message::Text(
            json!({"headers": sub_val["headers"], "errcode": 0, "errmsg": "ok"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        loop {
            tokio::select! {
                Some(out) = to_ws_rx.recv() => {
                    if ws.send(Message::Text(out.to_string().into())).await.is_err() {
                        break;
                    }
                }
                incoming = ws.next() => {
                    let Some(Ok(frame)) = incoming else { break };
                    if let Message::Text(t) = frame {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = from_ws_tx.send(v);
                        }
                    }
                }
            }
        }
    });

    let (st, created) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"name": "pc-im", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = created["token"].as_str().unwrap().to_owned();

    let (st, im_cfg) = req_json(
        &router,
        "PUT",
        "/api/integrations/im",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "wecom": {
                "enabled": true,
                "bot_id": "bot-im-approve",
                "secret": "sec-im-approve",
                "ws_url": format!("ws://{ws_addr}")
            },
            "issue_bind_code": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{im_cfg}");
    let bind_code = im_cfg["bind_code"].as_str().unwrap().to_owned();

    // Bind Alice's WeCom user via `/bind <code>` over WebSocket.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let (_, boot_im) = req_json(
                &router,
                "GET",
                "/api/integrations/im",
                Some(&alice_c),
                None,
                None,
            )
            .await;
            if boot_im["wecom"]["status"]["state"] == "connected" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("wecom ws connected");

    to_ws_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "wc_bind_1"},
            "body": {
                "msgid": "msg_bind_1",
                "chattype": "single",
                "chatid": "",
                "from": {"userid": "alice_wx"},
                "msgtype": "text",
                "text": {"content": format!("/bind {bind_code}")}
            }
        }))
        .unwrap();
    let bind_reply = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let f = from_ws_rx.recv().await.unwrap();
            if f["cmd"] == "aibot_respond_msg" && f["body"]["stream"]["finish"] == true {
                break f;
            }
        }
    })
    .await
    .unwrap();
    assert!(bind_reply["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("已成功绑定"));

    // Dispatch a task to Alice's node.
    let (st, chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "/node --agent opencode 推送发布分支"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let task_id = chat["tasks"][0]["task_id"].as_str().unwrap().to_owned();

    let w1_resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/node/v1/work")
                .header(header::HOST, "127.0.0.1:8080")
                .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                .header("x-agenct-node-protocol", "2")
                .header(
                    "x-agenct-node-env",
                    "kind:pc | os:linux | host:lab | agent:opencode | agents:opencode | sandbox:workspace-write | isolation:bwrap | elevated:1",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(w1_resp.status(), StatusCode::OK);
    let w1: Value =
        serde_json::from_slice(&w1_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let lease1 = w1["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev1 = w1["job"]["task_revision"].as_i64().unwrap();

    // Node submits failure with blocked_reason -> approval code issued and appended to assistant message.
    let sub1 = router
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
                        "lease_token": lease1,
                        "request_id": "sub-im-blk-1",
                        "expected_revision": rev1,
                        "status": "failed",
                        "content": "Host key verification failed.",
                        "agent": "opencode",
                        "blocked_reason": "Host key verification failed`，实际 `OUT_ERR`）。"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub1.status(), StatusCode::OK);

    let last_msg: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT content FROM messages WHERE user_id='u-alice' ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    let marker = "`/approve ";
    let idx = last_msg.find(marker).expect("approval command emitted");
    let code = last_msg[idx + marker.len()..idx + marker.len() + 6].to_owned();
    assert_eq!(code.len(), 6);
    assert!(code.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert!(
        last_msg.contains(&format!("`/approve {code}`")),
        "unmatched backticks corrupted approval markdown: {last_msg}"
    );
    assert!(
        !last_msg.contains("）。）。"),
        "duplicate trailing punctuation in footer: {last_msg}"
    );

    // Normal Chinese sentence starting with "放行" must NOT be treated as an approval command.
    let (st, chat_normal) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "放行这个是交付的时候权限授权是不"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !chat_normal["reply"].as_str().unwrap().contains("单次放行"),
        "natural sentence unexpectedly intercepted: {chat_normal}"
    );

    // Bob cannot consume Alice's code.
    let err_bob = agenct_hub::collab::consume_approval_code(&hub, "u-bob", &code)
        .await
        .err()
        .expect("cross-user code must fail");
    assert_eq!(err_bob.0, StatusCode::BAD_REQUEST);

    // Alice approves via WeCom long-connection using lowercase /approve <code> -> elevated job queued!
    to_ws_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "wc_approve_1"},
            "body": {
                "msgid": "msg_approve_1",
                "chattype": "single",
                "chatid": "",
                "from": {"userid": "alice_wx"},
                "msgtype": "text",
                "text": {"content": format!(" /approve {} ", code.to_ascii_lowercase())}
            }
        }))
        .unwrap();
    let app_reply = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let f = from_ws_rx.recv().await.unwrap();
            if f["cmd"] == "aibot_respond_msg"
                && f["headers"]["req_id"] == "wc_approve_1"
                && f["body"]["stream"]["finish"] == true
            {
                break f;
            }
        }
    })
    .await
    .unwrap();
    assert!(
        app_reply["body"]["stream"]["content"]
            .as_str()
            .unwrap()
            .contains("已授权单次放行重试"),
        "{app_reply}"
    );

    let w2_resp = router
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
    let w2: Value =
        serde_json::from_slice(&w2_resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(w2["job"]["task_id"], task_id);
    assert_eq!(w2["job"]["sandbox"], "elevated");
    assert_eq!(w2["job"]["authorization"], "manual");

    // Replaying the same approval code via Web chat must be rejected as already used.
    let (st, replay_res) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": format!("放行 {code}")})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        replay_res["reply"]
            .as_str()
            .unwrap()
            .contains("单次放行未生效"),
        "{replay_res}"
    );

    let audit_cnt: i64 = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT COUNT(*) FROM audit_logs WHERE action = 'task_elevate' AND detail LIKE '%chat_approve%'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(audit_cnt, 1);

    // Finish w2 first so the node queue is idle before testing proactive require_elevation.
    let lease2 = w2["job"]["lease_token"].as_str().unwrap().to_owned();
    let rev2 = w2["job"]["task_revision"].as_i64().unwrap();
    let sub2 = router
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
                        "lease_token": lease2,
                        "request_id": "sub-im-done-2",
                        "expected_revision": rev2,
                        "status": "completed",
                        "content": "pushed ok"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sub2.status(), StatusCode::OK);

    // Proactive Chat-First `require_elevation=true`: creates `pending` task + `/approve` code,
    // NEVER executes before human approval, and runs `elevated` right after `/approve`.
    let (st, req_elev_chat) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": "/node --require-elevation true git push origin master"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{req_elev_chat}");
    assert_eq!(req_elev_chat["tasks"][0]["status"], "pending");
    assert_eq!(req_elev_chat["tasks"][0]["sandbox"], "elevated");
    let pending_tid = req_elev_chat["tasks"][0]["task_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let reply2 = req_elev_chat["reply"].as_str().unwrap();
    let idx2 = reply2
        .find("`/approve ")
        .expect("guaranteed approval prompt in chat reply");
    let code2 = reply2[idx2 + "`/approve ".len()..idx2 + "`/approve ".len() + 6].to_owned();

    // Node polling BEFORE human approval MUST receive null job.
    let poll_before = router
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
    let pb: Value =
        serde_json::from_slice(&poll_before.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert!(
        pb["job"].is_null(),
        "pending elevated task leaked to node before approval: {pb}"
    );

    // Human approves via Web chat `/approve <code>` -> task enters queue with sandbox="elevated".
    let (st_app, app_res) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({"message": format!("/approve {code2}")})),
    )
    .await;
    assert_eq!(st_app, StatusCode::OK, "{app_res}");

    let poll_after = router
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
    let pa: Value =
        serde_json::from_slice(&poll_after.into_body().collect().await.unwrap().to_bytes())
            .unwrap();
    assert_eq!(pa["job"]["task_id"], pending_tid);
    assert_eq!(pa["job"]["sandbox"], "elevated");
    assert_eq!(pa["job"]["authorization"], "manual");

    // Web-initiated require_elevation also proactively notified Alice on her bound WeCom bot.
    let pushed = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let f = from_ws_rx.recv().await.unwrap();
            if f["cmd"] == "aibot_send_msg"
                && f["body"]["markdown"]["content"]
                    .as_str()
                    .is_some_and(|c| c.contains(&code2))
            {
                break f;
            }
        }
    })
    .await
    .expect("elevation approval code should push proactively to bound IM");
    assert_eq!(pushed["body"]["chatid"], "alice_wx");
}

#[tokio::test]
async fn wecom_and_dingtalk_ws_long_connection_bind_chat_and_proactive_push_work() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::{mpsc, Mutex};
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    let mut cfg = test_config("https://git.example.com");
    cfg.allow_private_outbound = true;
    let hub = Hub::open(cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // 1. Mock WeCom WS server.
    let wc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wc_addr = wc_listener.local_addr().unwrap();
    let (wc_in_tx, mut wc_in_rx) = mpsc::unbounded_channel::<Value>();
    let (wc_out_tx, mut wc_out_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((stream, _)) = wc_listener.accept().await else {
            return;
        };
        let mut ws = accept_async(stream).await.unwrap();
        let sub = ws.next().await.unwrap().unwrap();
        let sub_v: Value = serde_json::from_str(sub.to_text().unwrap()).unwrap();
        assert_eq!(sub_v["cmd"], "aibot_subscribe");
        assert_eq!(sub_v["body"]["bot_id"], "wc_bot_1");
        assert_eq!(sub_v["body"]["secret"], "wc_sec_1");
        ws.send(Message::Text(
            json!({"headers": sub_v["headers"], "errcode": 0, "errmsg": "ok"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        loop {
            tokio::select! {
                Some(v) = wc_in_rx.recv() => {
                    if ws.send(Message::Text(v.to_string().into())).await.is_err() {
                        break;
                    }
                }
                msg = ws.next() => {
                    let Some(Ok(frame)) = msg else { break };
                    if let Message::Text(t) = frame {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = wc_out_tx.send(v);
                        }
                    }
                }
            }
        }
    });

    // 2. Mock DingTalk WS server + HTTP gateway/OpenAPI/sessionWebhook server.
    let dd_ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dd_ws_addr = dd_ws_listener.local_addr().unwrap();
    let (dd_in_tx, mut dd_in_rx) = mpsc::unbounded_channel::<Value>();
    let (dd_out_tx, mut dd_out_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((stream, _)) = dd_ws_listener.accept().await else {
            return;
        };
        let mut ws = accept_async(stream).await.unwrap();
        loop {
            tokio::select! {
                Some(v) = dd_in_rx.recv() => {
                    if ws.send(Message::Text(v.to_string().into())).await.is_err() {
                        break;
                    }
                }
                msg = ws.next() => {
                    let Some(Ok(frame)) = msg else { break };
                    if let Message::Text(t) = frame {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = dd_out_tx.send(v);
                        }
                    }
                }
            }
        }
    });

    let dd_hooks = Arc::new(Mutex::new(Vec::<Value>::new()));
    let dd_batch = Arc::new(Mutex::new(Vec::<Value>::new()));
    let dd_http_app = Router::new()
        .route(
            "/v1.0/gateway/connections/open",
            post({
                let ws_ep = format!("ws://{dd_ws_addr}/connect");
                move |Json(body): Json<Value>| {
                    let ep = ws_ep.clone();
                    async move {
                        assert_eq!(body["clientId"], "dd_client_1");
                        assert_eq!(body["clientSecret"], "dd_sec_1");
                        Json(json!({"endpoint": ep, "ticket": "tkt+abc/123"}))
                    }
                }
            }),
        )
        .route(
            "/v1.0/oauth2/accessToken",
            post(|Json(body): Json<Value>| async move {
                assert_eq!(body["appKey"], "dd_client_1");
                Json(json!({"accessToken": "dd_tok_live", "expireIn": 7200}))
            }),
        )
        .route(
            "/v1.0/robot/oToMessages/batchSend",
            post({
                let cap = dd_batch.clone();
                move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        assert_eq!(
                            headers
                                .get("x-acs-dingtalk-access-token")
                                .and_then(|v| v.to_str().ok()),
                            Some("dd_tok_live")
                        );
                        cap.lock().await.push(body);
                        Json(json!({"processQueryKey": "pq_1"}))
                    }
                }
            }),
        )
        .route(
            "/session_hook",
            post({
                let cap = dd_hooks.clone();
                move |Json(body): Json<Value>| {
                    let cap = cap.clone();
                    async move {
                        cap.lock().await.push(body);
                        Json(json!({"errcode": 0, "errmsg": "ok"}))
                    }
                }
            }),
        );
    let dd_http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dd_http_addr = dd_http_listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(dd_http_listener, dd_http_app).await;
    });

    // Save both IM channels and get one-time bind code.
    let (st, saved) = req_json(
        &router,
        "PUT",
        "/api/integrations/im",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "wecom": {
                "enabled": true,
                "bot_id": "wc_bot_1",
                "secret": "wc_sec_1",
                "ws_url": format!("ws://{wc_addr}")
            },
            "dingtalk": {
                "enabled": true,
                "client_id": "dd_client_1",
                "client_secret": "dd_sec_1",
                "api_base": format!("http://{dd_http_addr}")
            }
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{saved}");
    let bind_code = saved["bind_code"].as_str().unwrap().to_owned();

    // Wait for both WS workers to connect.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let (_, cfg_get) = req_json(
                &router,
                "GET",
                "/api/integrations/im",
                Some(&cookie),
                None,
                None,
            )
            .await;
            if cfg_get["wecom"]["status"]["state"] == "connected"
                && cfg_get["dingtalk"]["status"]["state"] == "connected"
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("both wecom and dingtalk connected");

    // A) WeCom: stranger rejected, owner binds with `@Agenct /bind <code>`, chats, receives streamed reply.
    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "wc_stranger"},
            "body": {
                "msgid": "wc_m_0",
                "chattype": "single",
                "from": {"userid": "intruder"},
                "msgtype": "text",
                "text": {"content": "secret info"}
            }
        }))
        .unwrap();
    let stranger_resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "wc_stranger" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(stranger_resp["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("尚未绑定"));

    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "wc_bind"},
            "body": {
                "msgid": "wc_m_1",
                "chattype": "group",
                "chatid": "wc_grp_1",
                "from": {"userid": "lee_wc"},
                "msgtype": "text",
                "text": {"content": format!("@Agenct  /bind {bind_code} ")}
            }
        }))
        .unwrap();
    let wc_bind_resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "wc_bind" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(wc_bind_resp["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("已成功绑定"));

    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "wc_chat"},
            "body": {
                "msgid": "wc_m_2",
                "chattype": "single",
                "from": {"userid": "lee_wc"},
                "msgtype": "text",
                "text": {"content": "ping from wecom"}
            }
        }))
        .unwrap();
    let wc_chat_resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "wc_chat" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(wc_chat_resp["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("收到你的消息：ping from wecom"));

    // B) DingTalk: SYSTEM/ping echoes opaque, CALLBACK ACKs 200, binds via same code (until DingTalk bound too), chats via sessionWebhook.
    dd_in_tx
        .send(json!({
            "type": "SYSTEM",
            "headers": {"topic": "ping", "messageId": "dd_ping_1", "contentType": "application/json"},
            "data": "{\"opaque\":\"opq-99\"}"
        }))
        .unwrap();
    let dd_pong = tokio::time::timeout(std::time::Duration::from_secs(3), dd_out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dd_pong["code"], 200);
    assert_eq!(dd_pong["headers"]["messageId"], "dd_ping_1");
    assert_eq!(dd_pong["data"], "{\"opaque\":\"opq-99\"}");

    let dd_hook_url = format!("http://{dd_http_addr}/session_hook");
    dd_in_tx
        .send(json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "dd_cb_1"},
            "data": json!({
                "msgId": "dd_m_1",
                "senderStaffId": "lee_dd",
                "conversationType": "1",
                "conversationId": "dd_cid_1",
                "robotCode": "dd_rc_1",
                "sessionWebhook": dd_hook_url,
                "text": {"content": format!("绑定 {bind_code}")}
            }).to_string()
        }))
        .unwrap();
    let dd_ack1 = tokio::time::timeout(std::time::Duration::from_secs(3), dd_out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dd_ack1["code"], 200);
    assert_eq!(dd_ack1["headers"]["messageId"], "dd_cb_1");

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if !dd_hooks.lock().await.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("dingtalk bind confirmation posted to sessionWebhook");

    dd_in_tx
        .send(json!({
            "type": "CALLBACK",
            "headers": {"topic": "/v1.0/im/bot/messages/get", "messageId": "dd_cb_2"},
            "data": json!({
                "msgId": "dd_m_2",
                "senderStaffId": "lee_dd",
                "conversationType": "1",
                "conversationId": "dd_cid_1",
                "robotCode": "dd_rc_1",
                "sessionWebhook": format!("http://{dd_http_addr}/session_hook"),
                "text": {"content": "hello dingtalk"}
            }).to_string()
        }))
        .unwrap();
    let dd_ack2 = tokio::time::timeout(std::time::Duration::from_secs(3), dd_out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(dd_ack2["code"], 200);
    assert_eq!(dd_ack2["headers"]["messageId"], "dd_cb_2");

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if dd_hooks.lock().await.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("dingtalk chat reply posted to sessionWebhook");
    let hooks_snapshot = dd_hooks.lock().await.clone();
    assert!(hooks_snapshot[1]["markdown"]["text"]
        .as_str()
        .unwrap()
        .contains("收到你的消息：hello dingtalk"));

    // C) Proactive test push exercises BOTH WeCom `aibot_send_msg` and DingTalk OpenAPI `batchSend`.
    let (st_test, test_res) = req_json(
        &router,
        "POST",
        "/api/integrations/im/test",
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st_test, StatusCode::OK, "{test_res}");
    assert_eq!(test_res["delivered"], json!(["wecom", "dingtalk"]));

    let wc_push = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["cmd"] == "aibot_send_msg" {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(wc_push["body"]["chatid"], "lee_wc");
    assert_eq!(wc_push["body"]["chat_type"], 1);

    let batch_snapshot = dd_batch.lock().await.clone();
    assert_eq!(batch_snapshot.len(), 1);
    assert_eq!(batch_snapshot[0]["robotCode"], "dd_rc_1");
    assert_eq!(batch_snapshot[0]["userIds"], json!(["lee_dd"]));
    assert_eq!(batch_snapshot[0]["msgKey"], "sampleMarkdown");
    let parsed_param: Value =
        serde_json::from_str(batch_snapshot[0]["msgParam"].as_str().unwrap()).unwrap();
    assert!(parsed_param["text"].as_str().unwrap().contains("已连通"));
}

#[tokio::test]
async fn shared_wecom_bot_routes_group_colleagues_to_isolated_accounts_and_collab_pushes() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::sync::mpsc;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    let mut cfg = test_config("https://git.example.com");
    cfg.allow_private_outbound = true;
    let hub = Hub::open(cfg).unwrap();
    let (alice_c, alice_csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let (bob_c, bob_csrf) = seed_user(&hub, "u-bob", "bob", "Bob", false).await;
    let router = app(hub.clone());

    let wc_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let wc_addr = wc_listener.local_addr().unwrap();
    let (wc_in_tx, mut wc_in_rx) = mpsc::unbounded_channel::<Value>();
    let (wc_out_tx, mut wc_out_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let Ok((stream, _)) = wc_listener.accept().await else {
            return;
        };
        let mut ws = accept_async(stream).await.unwrap();
        let sub = ws.next().await.unwrap().unwrap();
        let sub_v: Value = serde_json::from_str(sub.to_text().unwrap()).unwrap();
        ws.send(Message::Text(
            json!({"headers": sub_v["headers"], "errcode": 0, "errmsg": "ok"})
                .to_string()
                .into(),
        ))
        .await
        .unwrap();
        loop {
            tokio::select! {
                Some(v) = wc_in_rx.recv() => {
                    if ws.send(Message::Text(v.to_string().into())).await.is_err() {
                        break;
                    }
                }
                msg = ws.next() => {
                    let Some(Ok(frame)) = msg else { break };
                    if let Message::Text(t) = frame {
                        if let Ok(v) = serde_json::from_str::<Value>(&t) {
                            let _ = wc_out_tx.send(v);
                        }
                    }
                }
            }
        }
    });

    // 1. Alice configures shared WeCom bot and binds `alice_wx`.
    let (st, alice_im) = req_json(
        &router,
        "PUT",
        "/api/integrations/im",
        Some(&alice_c),
        Some(&alice_csrf),
        Some(json!({
            "wecom": {
                "enabled": true,
                "bot_id": "wc_shared",
                "secret": "wc_shared_sec",
                "ws_url": format!("ws://{wc_addr}")
            },
            "issue_bind_code": true
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let alice_code = alice_im["bind_code"].as_str().unwrap().to_owned();

    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let (_, im_g) = req_json(
                &router,
                "GET",
                "/api/integrations/im",
                Some(&bob_c),
                None,
                None,
            )
            .await;
            if im_g["shared_wecom_online"] == true {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("bob should see shared_wecom_online=true");

    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_bind_a"},
            "body": {
                "msgid": "sh_m_bind_a",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "alice_wx"},
                "msgtype": "text",
                "text": {"content": format!("@Agenct /bind {alice_code}")}
            }
        }))
        .unwrap();
    let bind_a = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "sh_bind_a" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(bind_a["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("Alice (@alice)"));

    // 2. Unbound stranger `@Agenct` in group gets setup instructions and cannot touch Alice's space.
    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_unbound"},
            "body": {
                "msgid": "sh_m_unbound",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "charlie_wx"},
                "msgtype": "text",
                "text": {"content": "@Agenct /remember Charlie入侵"}
            }
        }))
        .unwrap();
    let unbound_resp = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "sh_unbound" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(unbound_resp["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("尚未绑定个人 Agenct 账号"));

    // 3. Bob (non-admin colleague, no bot credentials needed) issues his own bind code and binds `bob_wx`.
    let (st, bob_im) = req_json(
        &router,
        "PUT",
        "/api/integrations/im",
        Some(&bob_c),
        Some(&bob_csrf),
        Some(json!({"issue_bind_code": true})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{bob_im}");
    let bob_code = bob_im["bind_code"].as_str().unwrap().to_owned();

    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_bind_b"},
            "body": {
                "msgid": "sh_m_bind_b",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "bob_wx"},
                "msgtype": "text",
                "text": {"content": format!("@Agenct /bind {bob_code}")}
            }
        }))
        .unwrap();
    let bind_b = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["headers"]["req_id"] == "sh_bind_b" && v["body"]["stream"]["finish"] == true {
                break v;
            }
        }
    })
    .await
    .unwrap();
    assert!(bind_b["body"]["stream"]["content"]
        .as_str()
        .unwrap()
        .contains("Bob (@bob)"));

    // 4. Both speak in the same group: each message routes strictly to the sender's own isolated account.
    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_mem_a"},
            "body": {
                "msgid": "sh_m_mem_a",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "alice_wx"},
                "msgtype": "text",
                "text": {"content": "@Agenct /remember Alice机密123"}
            }
        }))
        .unwrap();
    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_mem_b"},
            "body": {
                "msgid": "sh_m_mem_b",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "bob_wx"},
                "msgtype": "text",
                "text": {"content": "@Agenct /remember Bob机密456"}
            }
        }))
        .unwrap();
    let mut got_a = false;
    let mut got_b = false;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !(got_a && got_b) {
            let v = wc_out_rx.recv().await.unwrap();
            if v["body"]["stream"]["finish"] == true {
                if v["headers"]["req_id"] == "sh_mem_a" {
                    got_a = true;
                } else if v["headers"]["req_id"] == "sh_mem_b" {
                    got_b = true;
                }
            }
        }
    })
    .await
    .unwrap();

    let (alice_mem, bob_mem): (String, String) = hub
        .db
        .read(|conn| {
            let a: String = conn.query_row(
                "SELECT memory_md FROM agent_profiles WHERE user_id='u-alice'",
                [],
                |r| r.get(0),
            )?;
            let b: String = conn.query_row(
                "SELECT memory_md FROM agent_profiles WHERE user_id='u-bob'",
                [],
                |r| r.get(0),
            )?;
            Ok((a, b))
        })
        .await
        .unwrap();
    assert!(
        alice_mem.contains("Alice机密123")
            && !alice_mem.contains("Bob机密456")
            && !alice_mem.contains("Charlie入侵")
    );
    assert!(bob_mem.contains("Bob机密456") && !bob_mem.contains("Alice机密123"));

    // 5. Alice delegates to `@bob` via shared WeCom group -> Bob receives 1v1 private push (`chatid=bob_wx, chat_type=1`).
    wc_in_tx
        .send(json!({
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "sh_del_a"},
            "body": {
                "msgid": "sh_m_del_a",
                "chattype": "group",
                "chatid": "grp_team",
                "from": {"userid": "alice_wx"},
                "msgtype": "text",
                "text": {"content": "@Agenct @bob 请协助核查接口"}
            }
        }))
        .unwrap();
    let bob_push = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let v = wc_out_rx.recv().await.unwrap();
            if v["cmd"] == "aibot_send_msg" && v["body"]["chatid"] == "bob_wx" {
                break v;
            }
        }
    })
    .await
    .expect("delegation should push 1v1 privately to Bob via shared WeCom WS");
    assert_eq!(bob_push["body"]["chat_type"], 1);

    // 6. Bob can also trigger `/api/integrations/im/test` using the shared WS connection.
    let (st_bob_test, bob_test_res) = req_json(
        &router,
        "POST",
        "/api/integrations/im/test",
        Some(&bob_c),
        Some(&bob_csrf),
        None,
    )
    .await;
    assert_eq!(st_bob_test, StatusCode::OK, "{bob_test_res}");
    assert_eq!(bob_test_res["delivered"], json!(["wecom"]));
}

#[tokio::test]
async fn dispatch_node_engine_override_does_not_leak_role_model() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let (st, node_res) = req_json(
        &router,
        "POST",
        "/api/nodes",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"name": "pc-ov", "kind": "pc"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let node_token = node_res["token"].as_str().unwrap().to_owned();

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "message": "/role executor agent=agy model=gemini-4-neon-high effort=high sandbox=workspace-write"
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 1. Overriding role's agent (agy -> opencode) without explicit model must drop role's agy model & effort.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --role executor --agent opencode 修复抽屉 z-index"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 2. Explicit model alongside engine override is honored.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --role executor --agent opencode --model deepseek/deepseek-flash 带显式模型"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    // 3. Same engine keeps role's configured model & effort.
    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/node --role executor 同引擎沿用角色模型"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let mut polled = Vec::new();
    for _ in 0..3 {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/node/v1/work")
                    .header(header::HOST, "127.0.0.1:8080")
                    .header(header::AUTHORIZATION, format!("Bearer {node_token}"))
                    .header("x-agenct-node-protocol", "2")
                    .header(
                        "x-agenct-node-env",
                        "kind:pc | os:linux | host:lab | agent:opencode | agents:agy,opencode",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body: Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let job = body["job"].clone();
        assert!(!job.is_null());
        polled.push(job);
    }

    assert_eq!(polled[0]["agent"], "opencode");
    assert!(
        polled[0]["model"].is_null(),
        "leaked model: {:?}",
        polled[0]
    );
    assert!(
        polled[0]["effort"].is_null(),
        "leaked effort: {:?}",
        polled[0]
    );

    assert_eq!(polled[1]["agent"], "opencode");
    assert_eq!(polled[1]["model"], "deepseek/deepseek-flash");

    assert_eq!(polled[2]["agent"], "agy");
    assert_eq!(polled[2]["model"], "gemini-4-neon-high");
    assert_eq!(polled[2]["effort"], "high");
}

#[tokio::test]
async fn chat_turn_completes_when_caller_future_is_dropped() {
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_millis(160)).await;
            let body = serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "后台继续完成后持久化成功"
                    }
                }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    let mut cfg = test_config("https://git.example.com");
    cfg.llm = LlmConfig::OpenAi {
        url: format!("http://{addr}/v1/chat/completions"),
        api_key: "test".into(),
        model: "mock".into(),
    };
    let hub = Hub::open(cfg).unwrap();
    let _ = seed_user(&hub, "u-alice", "alice", "Alice", true).await;

    // Simulate HTTP client aborting after 40ms while the mock LLM responds at 160ms.
    let timed = tokio::time::timeout(
        Duration::from_millis(40),
        agenct_hub::brain::run_turn(
            &hub,
            "u-alice",
            "hello detached",
            Some("req-detach-1"),
            None,
        ),
    )
    .await;
    assert!(timed.is_err(), "caller must drop before mock LLM replies");

    let mut completed_msg: Option<(i64, String)> = None;
    for _ in 0..80 {
        let row: Option<(i64, String)> = hub
            .db
            .read(|conn| {
                Ok(rusqlite::OptionalExtension::optional(conn.query_row(
                    "SELECT id, status FROM messages WHERE user_id='u-alice' AND request_id='req-detach-1'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ))?)
            })
            .await
            .unwrap();
        if let Some((mid, status)) = row {
            if status == "complete" {
                completed_msg = Some((mid, status));
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let (user_msg_id, status) = completed_msg.expect("detached turn must complete in background");
    assert_eq!(status, "complete");

    let reply: String = hub
        .db
        .read(move |conn| {
            Ok(conn.query_row(
                "SELECT content FROM messages WHERE user_id='u-alice' AND reply_to=?1 AND role='assistant'",
                [user_msg_id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(reply, "后台继续完成后持久化成功");

    // Replaying the same request_id returns the persisted reply instead of 409 Conflict.
    let replay = agenct_hub::brain::run_turn(
        &hub,
        "u-alice",
        "hello detached",
        Some("req-detach-1"),
        None,
    )
    .await
    .unwrap();
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["reply"], "后台继续完成后持久化成功");

    let _ = server.await;
}

#[tokio::test]
async fn chat_turn_marks_failed_on_upstream_error_and_allows_retry() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let _ = seed_user(&hub, "u-alice", "alice", "Alice", true).await;

    let err = agenct_hub::brain::run_turn(
        &hub,
        "u-alice",
        "/stub_fail",
        Some("req-fail-retry-1"),
        None,
    )
    .await
    .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_GATEWAY);

    let status: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT status FROM messages WHERE user_id='u-alice' AND request_id='req-fail-retry-1'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(status, "failed");

    let ok = agenct_hub::brain::run_turn(
        &hub,
        "u-alice",
        "retry works now",
        Some("req-fail-retry-1"),
        None,
    )
    .await
    .unwrap();
    assert!(ok["reply"].as_str().is_some_and(|s| !s.is_empty()));

    let status_after: String = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT status FROM messages WHERE user_id='u-alice' AND request_id='req-fail-retry-1'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(status_after, "complete");
}

#[tokio::test]
async fn chat_endpoint_returns_202_immediately_and_delivers_via_sse() {
    use std::time::{Duration, Instant};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let body = serde_json::json!({
                "choices": [{
                    "message": {
                        "role": "assistant",
                        "content": "202 真异步 SSE 交付成功"
                    }
                }]
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.shutdown().await;
        }
    });

    let mut cfg = test_config("https://git.example.com");
    cfg.llm = LlmConfig::OpenAi {
        url: format!("http://{addr}/v1/chat/completions"),
        api_key: "test".into(),
        model: "mock".into(),
    };
    let hub = Hub::open(cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());
    let mut sse_rx = hub.events.subscribe("u-alice");

    let started = Instant::now();
    let (st, acc) = req_json_raw(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "async ping", "request_id": "req-async-202"})),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{acc}");
    assert!(
        started.elapsed() < Duration::from_millis(150),
        "/api/chat should return 202 immediately without waiting for 200ms LLM"
    );
    assert_eq!(acc["status"], "accepted");
    assert_eq!(acc["request_id"], "req-async-202");
    let msg_id = acc["message_id"].as_i64().unwrap();
    assert!(msg_id > 0);

    // Same request_id while in-flight rejects with 409; concurrent turn also rejects with 409.
    let (st_dup, _) = req_json_raw(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "async ping", "request_id": "req-async-202"})),
    )
    .await;
    assert_eq!(st_dup, StatusCode::CONFLICT);

    let (st_other, _) = req_json_raw(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "concurrent msg", "request_id": "req-async-concurrent"})),
    )
    .await;
    assert_eq!(st_other, StatusCode::CONFLICT);

    // Wait for SSE completion event.
    let mut saw_done = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), sse_rx.recv()).await {
            if ev.kind == "message" {
                let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
                if data["done"] == true && data["message_id"] == msg_id {
                    assert_eq!(data["request_id"], "req-async-202");
                    saw_done = true;
                    break;
                }
            }
        }
    }
    assert!(saw_done, "must emit SSE message done event with message_id");

    let (bst, boot) =
        req_json_raw(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(bst, StatusCode::OK);
    let msgs = boot["messages"].as_array().unwrap();
    let u_row = msgs
        .iter()
        .find(|m| m["id"].as_i64() == Some(msg_id))
        .unwrap();
    assert_eq!(u_row["status"], "complete");
    let a_row = msgs
        .iter()
        .find(|m| m["role"] == "assistant" && m["reply_to"].as_i64() == Some(msg_id))
        .unwrap();
    assert_eq!(a_row["content"], "202 真异步 SSE 交付成功");

    // Replaying the same request_id returns 200 OK with cached reply.
    let (st_replay, replay) = req_json_raw(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "async ping", "request_id": "req-async-202"})),
    )
    .await;
    assert_eq!(st_replay, StatusCode::OK);
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["reply"], "202 真异步 SSE 交付成功");

    let _ = server.await;
}

#[tokio::test]
async fn chat_endpoint_async_failure_emits_sse_error_and_marks_failed() {
    use std::time::Duration;

    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());
    let mut sse_rx = hub.events.subscribe("u-alice");

    let (st, acc) = req_json_raw(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "/stub_fail", "request_id": "req-async-fail"})),
    )
    .await;
    assert_eq!(st, StatusCode::ACCEPTED, "{acc}");
    let msg_id = acc["message_id"].as_i64().unwrap();

    let mut failed_err: Option<String> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Ok(ev)) = tokio::time::timeout(Duration::from_millis(200), sse_rx.recv()).await {
            if ev.kind == "message" {
                let data: Value = serde_json::from_str(&ev.data).unwrap_or(Value::Null);
                if data["failed"] == true && data["message_id"] == msg_id {
                    assert_eq!(data["request_id"], "req-async-fail");
                    failed_err = data["error"].as_str().map(str::to_owned);
                    break;
                }
            }
        }
    }
    let err_str = failed_err.expect("must emit SSE message failed event");
    assert!(!err_str.is_empty());

    let db_status: String = hub
        .db
        .read(move |conn| {
            Ok(
                conn.query_row("SELECT status FROM messages WHERE id = ?1", [msg_id], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(db_status, "failed");
}

#[tokio::test]
async fn runtime_ssrf_guard_secrets_flag_and_local_login_rate_limit() {
    // 1. `secrets_encrypted` reflects AGENCT_MASTER_KEY configuration.
    let hub_plain = Hub::open(local_only_config()).unwrap();
    let router_plain = app(hub_plain.clone());
    let (st, cookie, body) = post_local_login(
        &router_plain,
        "local-test-token-0123456789",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let cookie = cookie.unwrap();
    let csrf = body["csrf_token"].as_str().unwrap().to_owned();
    let (st, settings_plain) = req_json(
        &router_plain,
        "GET",
        "/api/settings/models",
        Some(&cookie),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(settings_plain["secrets_encrypted"], false);

    let mut enc_cfg = local_only_config();
    enc_cfg.master_key = Some("0123456789abcdef0123456789abcdef".into());
    let hub_enc = Hub::open(enc_cfg).unwrap();
    let router_enc = app(hub_enc.clone());
    let (st, cookie_enc, body_enc) = post_local_login(
        &router_enc,
        "local-test-token-0123456789",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let (st, settings_enc) = req_json(
        &router_enc,
        "GET",
        "/api/settings/models",
        Some(&cookie_enc.unwrap()),
        None,
        None,
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(settings_enc["secrets_encrypted"], true);
    let _ = body_enc;

    // 2. Runtime SSRF re-check rejects persisted literal private/loopback URL when allow_private_outbound=false.
    hub_plain
        .db
        .write(|conn| {
            conn.execute(
                "INSERT INTO user_models(user_id, config_json, updated_at) VALUES (?1, ?2, 1)",
                rusqlite::params![
                    "local-owner",
                    r#"{"brain":{"url":"http://127.0.0.1:9/v1/chat/completions","api_key":"sk-x","model":"m"}}"#
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, err_body) = req_json(
        &router_plain,
        "POST",
        "/api/settings/models/test",
        Some(&cookie),
        Some(&csrf),
        None,
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "{err_body}");

    // 3. Sliding-window local login rate limit returns 429 after 10 failures without blocking a separate Hub.
    for attempt in 0..10 {
        let (st, _, _) =
            post_local_login(&router_plain, "wrong-token", Some("http://127.0.0.1:8080")).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    let (st, _, rate_body) =
        post_local_login(&router_plain, "wrong-token", Some("http://127.0.0.1:8080")).await;
    assert_eq!(st, StatusCode::TOO_MANY_REQUESTS, "{rate_body}");

    // Independent Hub instance's limiter remains unaffected.
    let (st, _, _) = post_local_login(
        &router_enc,
        "local-test-token-0123456789",
        Some("http://127.0.0.1:8080"),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn security_headers_attachment_isolation_and_approval_rate_limit() {
    let mut https_cfg = test_config("https://git.example.com");
    https_cfg.public_origin = "https://agenct.example.com".into();
    https_cfg.local_http = false;
    let hub = Hub::open(https_cfg).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    let upload_att = |name: &str, mime: &str, data: &str| {
        let r = router.clone();
        let ck = cookie.clone();
        let cs = csrf.clone();
        let body = json!({
            "name": name,
            "mime_type": mime,
            "data_base64": STANDARD.encode(data),
        })
        .to_string();
        async move {
            let resp = r
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/attachments")
                        .header(header::HOST, "agenct.example.com")
                        .header(header::ORIGIN, "https://agenct.example.com")
                        .header(header::COOKIE, format!("__Host-agenct-session={ck}"))
                        .header("x-csrf-token", cs)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            let status = resp.status();
            let bytes = resp.into_body().collect().await.unwrap().to_bytes();
            let val: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            (status, val)
        }
    };

    // 1. Standard HTTPS endpoints include HSTS, X-Frame-Options, Permissions-Policy, and page CSP.
    let resp = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .header(header::HOST, "agenct.example.com")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h["x-frame-options"], "DENY");
    assert_eq!(
        h["permissions-policy"],
        "camera=(), microphone=(), geolocation=()"
    );
    assert_eq!(
        h["strict-transport-security"],
        "max-age=31536000; includeSubDomains"
    );
    assert!(h["content-security-policy"]
        .to_str()
        .unwrap()
        .contains("default-src 'self'"));

    // 2. Non-image attachments force attachment disposition + sandbox CSP; images keep inline + sandbox CSP.
    let (st, txt_up) = upload_att("report.txt", "text/plain", "hello security").await;
    assert_eq!(st, StatusCode::OK, "{txt_up}");
    let txt_id = txt_up["id"].as_str().unwrap();
    let txt_dl = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/attachments/{txt_id}"))
                .header(header::HOST, "agenct.example.com")
                .header(header::COOKIE, format!("__Host-agenct-session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(txt_dl.status(), StatusCode::OK);
    assert!(txt_dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment;"));
    assert_eq!(
        txt_dl.headers()["content-security-policy"],
        "default-src 'none'; sandbox"
    );

    let png_bytes = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+X2ZkAAAAASUVORK5CYII=")
        .unwrap();
    let (st, png_up) = {
        let resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/attachments")
                    .header(header::HOST, "agenct.example.com")
                    .header(header::ORIGIN, "https://agenct.example.com")
                    .header(header::COOKIE, format!("__Host-agenct-session={cookie}"))
                    .header("x-csrf-token", &csrf)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({
                            "name": "shot.png",
                            "mime_type": "image/png",
                            "data_base64": STANDARD.encode(&png_bytes),
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let st = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            st,
            serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null),
        )
    };
    assert_eq!(st, StatusCode::OK, "{png_up}");
    let png_id = png_up["id"].as_str().unwrap();
    let png_dl = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/attachments/{png_id}"))
                .header(header::HOST, "agenct.example.com")
                .header(header::COOKIE, format!("__Host-agenct-session={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(png_dl.status(), StatusCode::OK);
    assert!(png_dl.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("inline;"));
    assert_eq!(
        png_dl.headers()["content-security-policy"],
        "default-src 'none'; sandbox"
    );

    // 3. `/approve` code verification enforces per-user rate limit (5 failures -> 429) and logs failures.
    for _ in 0..5 {
        let err = agenct_hub::collab::consume_approval_code(&hub, "u-alice", "WRONG1")
            .await
            .err()
            .expect("wrong approval code must fail");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }
    let blocked = agenct_hub::collab::consume_approval_code(&hub, "u-alice", "WRONG1")
        .await
        .err()
        .expect("6th attempt must be rate-limited");
    assert_eq!(blocked.0, StatusCode::TOO_MANY_REQUESTS);

    let denied_audits: i64 = hub
        .db
        .read(|conn| {
            Ok(conn.query_row(
                "SELECT count(*) FROM audit_logs WHERE user_id='u-alice' AND action='task_elevate_denied'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(denied_audits, 5);
}

#[tokio::test]
async fn visual_role_put_api_and_bootstrap_context_stats() {
    let hub = Hub::open(test_config("https://git.example.com")).unwrap();
    let (cookie, csrf) = seed_user(&hub, "u-alice", "alice", "Alice", true).await;
    let router = app(hub.clone());

    // 1. PUT /api/roles/{name} requires CSRF.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/roles/reviewer",
        Some(&cookie),
        None,
        Some(json!({
            "description": "只读审查",
            "agent": "claude",
            "effort": "high",
            "sandbox": "readonly",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::FORBIDDEN);

    // 2. Valid PUT creates/updates role and persists in /api/roles and /api/bootstrap.
    let (st, created) = req_json(
        &router,
        "PUT",
        "/api/roles/reviewer",
        Some(&cookie),
        Some(&csrf),
        Some(json!({
            "description": "只读代码审查",
            "agent": "claude",
            "model": "claude-opus-4-6",
            "effort": "high",
            "sandbox": "readonly",
            "node_id": "",
            "prompt": "只读检查 diff，不改文件",
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{created}");
    assert_eq!(created["name"], "reviewer");
    assert_eq!(created["agent"], "claude");
    assert_eq!(created["effort"], "high");
    assert_eq!(created["sandbox"], "readonly");

    // 3. Rejects sandbox escalation, invalid model flag, and unknown node_id.
    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/roles/reviewer",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"sandbox": "unsafe-full"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/roles/reviewer",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"model": "-bad-flag"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    let (st, _) = req_json(
        &router,
        "PUT",
        "/api/roles/reviewer",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"node_id": "nonexistent-node"})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);

    // 4. /api/bootstrap exposes honest context_stats and reflects chat turns.
    let (st, boot0) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let stats0 = &boot0["context_stats"];
    assert!(stats0["soul_bytes"].as_u64().unwrap() > 0);
    assert!(stats0["memory_bytes"].as_u64().unwrap() > 0);
    assert_eq!(stats0["compact_trigger_messages"], 18);
    assert_eq!(stats0["uncompacted_messages"], 0);
    assert_eq!(stats0["has_rolling_summary"], false);

    let (st, _) = req_json(
        &router,
        "POST",
        "/api/chat",
        Some(&cookie),
        Some(&csrf),
        Some(json!({"message": "hello context meter"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    let (st, boot1) = req_json(&router, "GET", "/api/bootstrap", Some(&cookie), None, None).await;
    assert_eq!(st, StatusCode::OK);
    let stats1 = &boot1["context_stats"];
    assert_eq!(stats1["uncompacted_messages"], 2);
    assert!(stats1["uncompacted_bytes"].as_u64().unwrap() > 0);
    assert_eq!(boot1["roles"].as_array().unwrap().len(), 1);
    assert_eq!(boot1["roles"][0]["name"], "reviewer");
}
