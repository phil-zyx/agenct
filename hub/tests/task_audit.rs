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
async fn lifecycle_is_replayable_and_delivery_versions_link_without_private_data() {
    let app = setup();
    let task = confirmed_task(&app, "alice", "bob", "Review public docs").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    for (revision, who, action, result) in [
        (0, "bob", "accept", None),
        (1, "bob", "deliver", Some("first result")),
        (2, "alice", "return", None),
        (3, "bob", "accept", None),
        (4, "bob", "deliver", Some("second result")),
        (5, "alice", "approve", None),
    ] {
        let (status, _) = request(
            &app,
            who,
            "POST",
            &format!("{path}/actions"),
            json!({"action":action,"expected_revision":revision,"result":result}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, detail) = request(&app, "alice", "GET", &path, Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        detail["timeline"],
        json!({"events":detail["events"], "deliveries":detail["deliveries"], "audit":detail["audit"]})
    );
    assert_eq!(detail["audit"]["schema_version"], 1);
    assert_eq!(detail["audit"]["legacy_history_incomplete"], false);
    assert_eq!(detail["audit"]["legacy_notice"], Value::Null);
    let events = detail["events"].as_array().unwrap();
    assert_eq!(events.len(), 7);
    let mut previous = Value::Null;
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["seq"], index + 1);
        assert_eq!(event["revision"], index);
        assert_eq!(event["provenance"], "recorded");
        assert_eq!(event["from_status"], previous);
        previous = event["to_status"].clone();
        let mut fields = event
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        fields.sort();
        assert_eq!(
            fields,
            vec![
                "action",
                "actor",
                "at",
                "delivery_seq",
                "from_status",
                "provenance",
                "revision",
                "seq",
                "to_status"
            ]
        );
    }
    assert_eq!(previous, detail["task"]["status"]);
    assert_eq!(events[2]["delivery_seq"], 1);
    assert_eq!(events[5]["delivery_seq"], 2);
    for index in [0, 1, 3, 4, 6] {
        assert_eq!(events[index]["delivery_seq"], Value::Null);
    }
    assert_eq!(detail["deliveries"][0]["content"], "first result");
    assert_eq!(detail["deliveries"][1]["content"], "second result");
    assert_eq!(
        request(&app, "bob", "GET", &path, Value::Null).await.1,
        detail
    );
    assert_eq!(
        request(&app, "eve", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    // Rejected stale replay cannot append a second event or delivery.
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"deliver","expected_revision":4,"result":"retry"})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        request(&app, "alice", "GET", &path, Value::Null).await.1,
        detail
    );
}

#[tokio::test]
async fn unrelated_private_proposals_do_not_change_task_event_sequences() {
    let app = setup();
    let task = confirmed_task(&app, "alice", "bob", "shared").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    let before = request(&app, "bob", "GET", &path, Value::Null).await.1;
    let proposal = request(
        &app,
        "alice",
        "POST",
        "/api/proposals",
        json!({"owner":"eve","summary":"private secret"}),
    )
    .await
    .1;
    request(
        &app,
        "alice",
        "POST",
        &format!(
            "/api/proposals/{}/actions",
            proposal["id"].as_str().unwrap()
        ),
        json!({"action":"reject","expected_revision":0}),
    )
    .await;
    confirmed_task(&app, "alice", "eve", "unrelated shared task").await;
    assert_eq!(
        request(&app, "bob", "GET", &path, Value::Null).await.1,
        before
    );
    request(
        &app,
        "bob",
        "POST",
        &format!("{path}/actions"),
        json!({"action":"cancel","expected_revision":0,"reason":"This task is no longer needed"}),
    )
    .await;
    let detail = request(&app, "bob", "GET", &path, Value::Null).await.1;
    assert_eq!(detail["events"][1]["seq"], 2);
    assert_eq!(detail["events"][1]["action"], "cancel");
    assert!(!detail.to_string().contains("private secret"));
}

#[tokio::test]
async fn event_write_failure_rolls_back_delivery_and_transition_then_reopen_replays() {
    let directory = std::env::temp_dir().join(format!("agenct-audit-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("hub.db");
    let cfg = config(file.to_str().unwrap());
    let app = app(Hub::open(cfg).unwrap());
    let task = confirmed_task(&app, "alice", "bob", "atomicity").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    let before = request(&app, "alice", "GET", &path, Value::Null).await.1;
    let db = rusqlite::Connection::open(&file).unwrap();
    db.execute_batch("CREATE TRIGGER fail_audit BEFORE INSERT ON events WHEN NEW.action='deliver' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
    assert_eq!(
        request(
            &app,
            "bob",
            "POST",
            &format!("{path}/actions"),
            json!({"action":"deliver","expected_revision":1,"result":"must rollback"})
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
        db.query_row("SELECT count(*) FROM deliveries", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    db.execute_batch("DROP TRIGGER fail_audit").unwrap();
    drop(db);
    drop(app);
    let reopened = agenct_hub::app(Hub::open(config(file.to_str().unwrap())).unwrap());
    assert_eq!(
        request(&reopened, "bob", "GET", &path, Value::Null).await.1,
        before
    );
    drop(reopened);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn replay_from_stored_events_and_deliveries_matches_current_task() {
    let directory = std::env::temp_dir().join(format!("agenct-replay-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let file = directory.join("hub.db");
    let router = app(Hub::open(config(file.to_str().unwrap())).unwrap());
    let task = confirmed_task(&router, "alice", "bob", "replay").await;
    let id = task["id"].as_str().unwrap();
    let path = format!("/api/tasks/{id}/actions");
    for (revision, who, action, result) in [
        (0, "bob", "accept", None),
        (1, "bob", "deliver", Some("version one")),
        (2, "alice", "return", None),
        (3, "bob", "accept", None),
        (4, "bob", "deliver", Some("version two")),
        (5, "alice", "return", None),
        (6, "bob", "accept", None),
        (7, "bob", "deliver", Some("version three")),
        (8, "alice", "approve", None),
    ] {
        assert_eq!(
            request(
                &router,
                who,
                "POST",
                &path,
                json!({"action":action,"expected_revision":revision,"result":result})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let db = rusqlite::Connection::open(&file).unwrap();
    db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
    assert_eq!(db.query_row("SELECT count(*) FROM pragma_foreign_key_list('events') WHERE \"table\"='tasks' AND \"from\"='task_id' AND \"to\"='id'",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert!(db
        .execute_batch(
            "INSERT INTO events(task_id,actor,action) VALUES ('missing','alice','created')"
        )
        .is_err());
    let mut status: Option<String> = None;
    let mut result: Option<String> = None;
    let mut revision = -1;
    // Reconstruct using only persisted audit and immutable delivery rows. The
    // tasks table is read only after reconstruction, as the acceptance oracle.
    let mut query=db.prepare("SELECT from_status,to_status,revision,delivery_seq,action FROM events WHERE task_id=?1 ORDER BY id").unwrap();
    let events = query
        .query_map([id], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .unwrap();
    for event in events {
        let (from, to, next, delivery, action) = event.unwrap();
        assert_eq!(from, status);
        assert_eq!(next, revision + 1);
        status = Some(to);
        revision = next;
        if action == "deliver" {
            result=Some(db.query_row("SELECT content FROM deliveries WHERE task_id=?1 AND status='submitted' AND seq=?2",rusqlite::params![id,delivery.unwrap()],|r|r.get(0)).unwrap());
        } else {
            assert!(delivery.is_none());
        }
        if action == "return" {
            result = None;
        }
    }
    let actual: (String, Option<String>, i64) = db
        .query_row(
            "SELECT status,result,revision FROM tasks WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!((status.unwrap(), result, revision), actual);
    drop(query);
    drop(db);
    drop(router);
    std::fs::remove_dir_all(directory).unwrap();
}

#[tokio::test]
async fn explicit_timeline_retains_three_submissions_and_two_returns() {
    let app = setup();
    let task = confirmed_task(&app, "alice", "bob", "Iterative review").await;
    let path = format!("/api/tasks/{}", task["id"].as_str().unwrap());
    for cycle in 0..3 {
        let revision = cycle * 3;
        for (who, action, expected, result) in [
            ("bob", "accept", revision, Value::Null),
            (
                "bob",
                "deliver",
                revision + 1,
                json!(format!("reviewed version {}", cycle + 1)),
            ),
        ] {
            assert_eq!(
                request(
                    &app,
                    who,
                    "POST",
                    &format!("{path}/actions"),
                    json!({"action":action,"expected_revision":expected,"result":result})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
        if cycle < 2 {
            assert_eq!(
                request(
                    &app,
                    "alice",
                    "POST",
                    &format!("{path}/actions"),
                    json!({"action":"return","expected_revision":revision+2})
                )
                .await
                .0,
                StatusCode::OK
            );
        }
    }
    let detail = request(&app, "alice", "GET", &path, Value::Null).await.1;
    let timeline = &detail["timeline"];
    assert_eq!(timeline["audit"]["legacy_history_incomplete"], false);
    assert_eq!(timeline["deliveries"].as_array().unwrap().len(), 3);
    for (index, delivery) in timeline["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        assert_eq!(delivery["seq"], index + 1);
        assert_eq!(
            delivery["content"],
            format!("reviewed version {}", index + 1)
        );
    }
    assert_eq!(
        timeline["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["action"] == "return")
            .count(),
        2
    );
    assert_eq!(
        request(&app, "bob", "GET", &path, Value::Null).await.1["timeline"],
        *timeline
    );
    assert_eq!(
        request(&app, "eve", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}
