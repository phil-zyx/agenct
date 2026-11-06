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
async fn in_state(app: &Router, state: &str) -> Value {
    let mut task = confirmed_task(app, "alice", "bob", "public work").await;
    let path = format!("/api/tasks/{}/actions", task["id"].as_str().unwrap());
    if state != "awaiting_owner" && state != "canceled" {
        assert_eq!(
            request(
                app,
                "bob",
                "POST",
                &path,
                json!({"action":"accept","expected_revision":0})
            )
            .await
            .0,
            StatusCode::OK
        );
        if state != "working" {
            assert_eq!(request(app,"bob","POST",&path,json!({"action":"deliver","expected_revision":1,"result":"retained public result"})).await.0,StatusCode::OK);
            if state == "completed" {
                assert_eq!(
                    request(
                        app,
                        "alice",
                        "POST",
                        &path,
                        json!({"action":"approve","expected_revision":2})
                    )
                    .await
                    .0,
                    StatusCode::OK
                );
            }
        }
    } else if state == "canceled" {
        assert_eq!(
            request(
                app,
                "alice",
                "POST",
                &path,
                json!({"action":"cancel","expected_revision":0,"reason":"earlier reason"})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    task = request(
        app,
        "alice",
        "GET",
        path.trim_end_matches("/actions"),
        Value::Null,
    )
    .await
    .1["task"]
        .clone();
    assert_eq!(task["status"], state);
    task
}

#[tokio::test]
async fn exact_cancel_identity_state_matrix_requires_shared_reason_and_retains_evidence() {
    for state in [
        "awaiting_owner",
        "working",
        "ready_for_acceptance",
        "completed",
        "canceled",
    ] {
        for actor in ["alice", "bob", "eve", "mallory"] {
            let app = setup();
            let task = in_state(&app, state).await;
            let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
            let before = request(&app, "alice", "GET", &path, Value::Null).await.1;
            let (status,_)=request(&app,actor,"POST",&format!("{path}/actions"),json!({"action":"cancel","expected_revision":task["revision"],"reason":"  Explicit shared reason <script>literal</script>  "})).await;
            let allowed = (actor == "alice"
                && ["awaiting_owner", "working", "ready_for_acceptance"].contains(&state))
                || (actor == "bob" && ["awaiting_owner", "working"].contains(&state));
            let expected = if actor == "mallory" {
                StatusCode::UNAUTHORIZED
            } else if actor == "eve" {
                StatusCode::NOT_FOUND
            } else if allowed {
                StatusCode::OK
            } else {
                StatusCode::CONFLICT
            };
            assert_eq!(status, expected, "{actor}/{state}");
            let after = request(&app, "alice", "GET", &path, Value::Null).await.1;
            if !allowed {
                assert_eq!(after, before);
                continue;
            }
            assert_eq!(after["task"]["status"], "canceled");
            assert_eq!(after["task"]["result"], before["task"]["result"]);
            assert_eq!(after["deliveries"], before["deliveries"]);
            let event = after["events"].as_array().unwrap().last().unwrap();
            assert_eq!(event["actor"], actor);
            assert_eq!(event["from_status"], state);
            assert_eq!(
                event["reason"],
                "Explicit shared reason <script>literal</script>"
            );
            assert_eq!(event["to_status"], "canceled");
            assert_eq!(event["delivery_seq"], Value::Null);
            assert_eq!(
                request(&app, "bob", "GET", &path, Value::Null).await.1,
                after
            );
            assert_eq!(
                request(&app, "eve", "GET", &path, Value::Null).await.0,
                StatusCode::NOT_FOUND
            );
            let replay=request(&app,actor,"POST",&format!("{path}/actions"),json!({"action":"cancel","expected_revision":task["revision"],"reason":"retry reason"})).await;
            assert_eq!(replay.0, StatusCode::CONFLICT);
            assert_eq!(
                request(&app, "alice", "GET", &path, Value::Null).await.1,
                after
            );
        }
    }
}

#[tokio::test]
async fn missing_blank_long_and_wrong_typed_reasons_never_mutate_and_unicode_boundary_works() {
    let app = setup();
    let task = in_state(&app, "working").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let before = request(&app, "alice", "GET", &path, Value::Null).await.1;
    for reason in [
        Value::Null,
        json!(""),
        json!(" \n\t "),
        json!("x".repeat(501)),
        json!("🦀".repeat(501)),
    ] {
        let status = request(
            &app,
            "alice",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"cancel","expected_revision":1,"reason":reason}),
        )
        .await
        .0;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            request(&app, "alice", "GET", &path, Value::Null).await.1,
            before
        );
    }
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"cancel","expected_revision":1})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"cancel","expected_revision":1,"reason":123})
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    assert_eq!(request(&app,"bob","POST",&format!("{path}/actions"),json!({"action":"deliver","expected_revision":1,"result":"x","reason":"not a cancellation"})).await.0,StatusCode::BAD_REQUEST);
    assert_eq!(
        request(&app, "alice", "GET", &path, Value::Null).await.1,
        before
    );
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"cancel","expected_revision":1,"reason":"🦀".repeat(500)})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn cancellation_reason_task_and_worker_revocation_commit_or_rollback_together() {
    let directory = std::env::temp_dir().join(format!("agenct-cancel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("hub.db");
    let app = app(Hub::open(config(file.to_str().unwrap())).unwrap());
    let task = in_state(&app, "working").await;
    let id = task["id"].as_str().unwrap();
    let path = format!("/api/tasks/{id}");
    let db = rusqlite::Connection::open(&file).unwrap();
    db.execute("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,lease_hash,lease_until,created_at) VALUES ('job','bob',?1,1,'repo','base','request','running','lease',999,0)",[id]).unwrap();
    db.execute("INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision) VALUES ('private',?1,1,'bob','PRIVATE DRAFT MUST NOT LEAK','draft',1)",[id]).unwrap();
    db.execute_batch("CREATE TRIGGER fail_revocation BEFORE UPDATE ON worker_jobs BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    let before = request(&app, "alice", "GET", &path, Value::Null).await.1;
    let body = json!({"action":"cancel","expected_revision":1,"reason":"User chose to stop collaboration"});
    assert_eq!(
        request(
            &app,
            "alice",
            "POST",
            &format!("{path}/actions"),
            body.clone()
        )
        .await
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        request(&app, "alice", "GET", &path, Value::Null).await.1,
        before
    );
    assert_eq!(
        db.query_row("SELECT lease_hash FROM worker_jobs", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "lease"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM events WHERE reason IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_revocation").unwrap();
    assert_eq!(
        request(&app, "alice", "POST", &format!("{path}/actions"), body)
            .await
            .0,
        StatusCode::OK
    );
    let state: (String, Option<String>, Option<i64>) = db
        .query_row(
            "SELECT status,lease_hash,lease_until FROM worker_jobs",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(state, ("cancelled".into(), None, None));
    let after = request(&app, "alice", "GET", &path, Value::Null).await.1;
    assert!(!after.to_string().contains("PRIVATE DRAFT"));
    assert_eq!(
        db.query_row(
            "SELECT content FROM deliveries WHERE id='private'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "PRIVATE DRAFT MUST NOT LEAK"
    );
    drop(db);
    drop(app);
    let reopened = agenct_hub::app(Hub::open(config(file.to_str().unwrap())).unwrap());
    assert_eq!(
        request(&reopened, "alice", "GET", &path, Value::Null)
            .await
            .1,
        after
    );
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn concurrent_delivery_and_cancellation_have_one_revision_bound_winner() {
    let app = setup();
    let task = in_state(&app, "working").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let actions = format!("{path}/actions");
    let (cancel, delivery) = tokio::join!(
        request(
            &app,
            "alice",
            "POST",
            &actions,
            json!({"action":"cancel","expected_revision":1,"reason":"Stop this task"})
        ),
        request(
            &app,
            "bob",
            "POST",
            &actions,
            json!({"action":"deliver","expected_revision":1,"result":"concurrent delivery"})
        )
    );
    assert!(
        (cancel.0 == StatusCode::OK && delivery.0 == StatusCode::CONFLICT)
            || (delivery.0 == StatusCode::OK && cancel.0 == StatusCode::CONFLICT)
    );
    let detail = request(&app, "alice", "GET", &path, Value::Null).await.1;
    assert_eq!(detail["task"]["revision"], 2);
    assert_eq!(detail["events"].as_array().unwrap().len(), 3);
    let event = detail["events"].as_array().unwrap().last().unwrap();
    if cancel.0 == StatusCode::OK {
        assert_eq!(event["reason"], "Stop this task");
        assert_eq!(detail["deliveries"], json!([]));
    } else {
        assert!(event.get("reason").is_none());
        assert_eq!(detail["deliveries"][0]["content"], "concurrent delivery");
    }
}
