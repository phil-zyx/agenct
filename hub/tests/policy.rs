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

struct Fixture {
    app: Router,
    db: rusqlite::Connection,
    directory: std::path::PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agenct-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("hub.db");
        let hub = Hub::open(config(file.to_str().unwrap()))
            .unwrap()
            .with_worker_scopes(vec![agenct_hub::worker_api::Scope {
                task_id: "task".into(),
                owner: "bob".into(),
                task_revision: 1,
                repository: "repo".into(),
                base_commit: "a".repeat(40),
            }])
            .unwrap();
        let db = rusqlite::Connection::open(file).unwrap();
        db.execute_batch("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','shared','working',1);
            INSERT INTO proposals(id,requester,owner,summary,status,revision,request_id,source) VALUES ('proposal','alice','bob','shared','pending',0,'proposal-key','explicit');").unwrap();
        Self {
            app: app(hub),
            db,
            directory,
        }
    }
    fn snapshot(&self) -> Vec<String> {
        [
            "SELECT json_array(id,status,revision,result) FROM tasks ORDER BY id",
            "SELECT json_array(id,status,revision,task_id) FROM proposals ORDER BY id",
            "SELECT json_array(task_id,actor,action) FROM events ORDER BY id",
            "SELECT json_array(id,status,lease_hash,lease_until) FROM worker_jobs ORDER BY id",
        ]
        .iter()
        .flat_map(|sql| {
            self.db
                .prepare(sql)
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        })
        .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
#[tokio::test]
async fn task_route_negative_identity_state_revision_matrix_never_mutates() {
    let f = Fixture::new();
    for state in [
        "awaiting_owner",
        "working",
        "ready_for_acceptance",
        "completed",
        "canceled",
        "legacy_unknown",
    ] {
        f.db.execute("UPDATE tasks SET status=?1 WHERE id='task'", [state])
            .unwrap();
        for who in ["alice", "bob", "eve", "mallory"] {
            for action in [
                "accept", "deliver", "approve", "return", "cancel", "unknown",
            ] {
                for revision in [0, 1] {
                    let allowed = revision == 1
                        && (who == "alice" || who == "bob")
                        && match action {
                            "accept" => who == "bob" && state == "awaiting_owner",
                            "deliver" => who == "bob" && state == "working",
                            "approve" | "return" => {
                                who == "alice" && state == "ready_for_acceptance"
                            }
                            "cancel" => {
                                ["awaiting_owner", "working"].contains(&state)
                                    || (who == "alice" && state == "ready_for_acceptance")
                            }
                            _ => false,
                        };
                    if allowed {
                        continue;
                    }
                    let before = f.snapshot();
                    let result = request(
                        &f.app,
                        who,
                        "POST",
                        "/api/tasks/task/actions",
                        json!({"action":action,"expected_revision":revision,"result":"delivery"}),
                    )
                    .await;
                    let expected = if who == "mallory" {
                        StatusCode::UNAUTHORIZED
                    } else if who == "eve" {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::CONFLICT
                    };
                    assert_eq!(result.0, expected, "{who}/{state}/{action}/{revision}");
                    assert_eq!(f.snapshot(), before);
                }
            }
        }
    }
}
#[tokio::test]
async fn proposal_route_negative_identity_state_revision_matrix_never_mutates() {
    let f = Fixture::new();
    for (state, revision) in [
        ("pending", 0),
        ("confirmed", 1),
        ("rejected", 1),
        ("unknown", 1),
    ] {
        f.db.execute(
            "UPDATE proposals SET status=?1,revision=?2 WHERE id='proposal'",
            rusqlite::params![state, revision],
        )
        .unwrap();
        for who in ["alice", "bob", "eve", "mallory"] {
            for action in ["confirm", "reject", "unknown"] {
                for reviewed in [0, 1, 2] {
                    let target = if action == "confirm" {
                        "confirmed"
                    } else {
                        "rejected"
                    };
                    let allowed = who == "alice"
                        && action != "unknown"
                        && ((state == "pending" && reviewed == revision)
                            || (state == target && revision == 1 && reviewed == 0));
                    if allowed {
                        continue;
                    }
                    let before = f.snapshot();
                    let result = request(
                        &f.app,
                        who,
                        "POST",
                        "/api/proposals/proposal/actions",
                        json!({"action":action,"expected_revision":reviewed}),
                    )
                    .await;
                    let expected = if who == "mallory" {
                        StatusCode::UNAUTHORIZED
                    } else if action == "unknown" {
                        StatusCode::BAD_REQUEST
                    } else if who != "alice" {
                        StatusCode::NOT_FOUND
                    } else {
                        StatusCode::CONFLICT
                    };
                    assert_eq!(result.0, expected, "{who}/{state}/{action}/{reviewed}");
                    assert_eq!(f.snapshot(), before);
                }
            }
        }
    }
}
#[tokio::test]
async fn worker_routes_negative_identity_task_and_job_state_matrix_never_mutates() {
    let f = Fixture::new();
    for state in [
        "awaiting_owner",
        "working",
        "ready_for_acceptance",
        "completed",
        "canceled",
    ] {
        f.db.execute("UPDATE tasks SET status=?1 WHERE id='task'", [state])
            .unwrap();
        for who in ["alice", "bob", "eve", "mallory"] {
            for revision in [0, 1] {
                if who == "bob" && state == "working" && revision == 1 {
                    continue;
                }
                let before = f.snapshot();
                let result=request(&f.app,who,"POST","/api/tasks/task/worker",json!({"expected_revision":revision,"repository":"repo","base_commit":"a".repeat(40),"request_id":"queue","confirm_queue_only":true})).await;
                let expected = if who == "mallory" {
                    StatusCode::UNAUTHORIZED
                } else if who != "bob" {
                    StatusCode::NOT_FOUND
                } else if revision != 1 {
                    StatusCode::CONFLICT
                } else {
                    StatusCode::NOT_FOUND
                };
                assert_eq!(result.0, expected, "{who}/{state}/{revision}");
                assert_eq!(f.snapshot(), before);
            }
        }
    }
    f.db.execute_batch("INSERT INTO worker_jobs(id,owner,task_id,task_revision,repository,base_commit,request_id,status,created_at) VALUES ('job','bob','task',1,'repo','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','queue','queued',0)").unwrap();
    for status in ["queued", "running", "review_ready", "cancelled", "failed"] {
        f.db.execute("UPDATE worker_jobs SET status=?1 WHERE id='job'", [status])
            .unwrap();
        for who in ["alice", "bob", "eve", "mallory"] {
            if who == "bob" && status != "failed" {
                continue;
            }
            let before = f.snapshot();
            let result = request(
                &f.app,
                who,
                "POST",
                "/api/tasks/task/worker/job/cancel",
                json!({}),
            )
            .await;
            assert_eq!(
                result.0,
                if who == "mallory" {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::NOT_FOUND
                }
            );
            assert_eq!(f.snapshot(), before);
        }
    }
}

#[tokio::test]
async fn unknown_task_state_cancellation_fails_closed_for_both_participants() {
    // Unknown/future states require an explicit policy rather than a catch-all.
    for who in ["alice", "bob"] {
        let f = Fixture::new();
        f.db.execute(
            "UPDATE tasks SET status='legacy_unknown' WHERE id='task'",
            [],
        )
        .unwrap();
        let before = f.snapshot();
        let response = request(
            &f.app,
            who,
            "POST",
            "/api/tasks/task/actions",
            json!({"action":"cancel","expected_revision":1,"reason":"This task is no longer needed"}),
        )
        .await;
        assert_eq!(response.0, StatusCode::CONFLICT);
        assert_eq!(f.snapshot(), before);
    }
}
#[tokio::test]
async fn representative_policy_errors_preserve_existing_contract() {
    let f = Fixture::new();
    for (who, path, body, expected) in [
        (
            "eve",
            "/api/tasks/task/actions",
            json!({"action":"cancel","expected_revision":1,"reason":"This task is no longer needed"}),
            "not found",
        ),
        (
            "bob",
            "/api/tasks/task/actions",
            json!({"action":"deliver","expected_revision":0,"result":"x"}),
            "task changed; refresh and review before acting",
        ),
        (
            "alice",
            "/api/tasks/task/actions",
            json!({"action":"deliver","expected_revision":1,"result":"x"}),
            "action not allowed for identity or current state",
        ),
        (
            "alice",
            "/api/proposals/proposal/actions",
            json!({"action":"unknown","expected_revision":0}),
            "unsupported proposal action",
        ),
        (
            "alice",
            "/api/proposals/proposal/actions",
            json!({"action":"confirm","expected_revision":1}),
            "proposal changed; refresh and review",
        ),
        (
            "bob",
            "/api/tasks/task/worker",
            json!({"expected_revision":1,"repository":"repo","base_commit":"a".repeat(40),"request_id":"queue","confirm_queue_only":false}),
            "explicit queue-only confirmation required",
        ),
        (
            "bob",
            "/api/tasks/task/worker",
            json!({"expected_revision":0,"repository":"repo","base_commit":"a".repeat(40),"request_id":"queue","confirm_queue_only":true}),
            "request does not match operator-approved task scope",
        ),
    ] {
        let before = f.snapshot();
        let response = request(&f.app, who, "POST", path, body).await;
        assert!(response.0.is_client_error());
        assert_eq!(response.1, json!({"error":expected}));
        assert_eq!(f.snapshot(), before);
    }
}
