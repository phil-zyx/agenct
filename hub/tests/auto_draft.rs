//! Automatic owner-private drafting when a task enters `working`.
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
struct Fixture {
    app: Router,
    dir: std::path::PathBuf,
    dispatcher: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.dispatcher.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
impl Fixture {
    fn new(auto: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("agenct-auto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: dir.join("hub.db"),
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
            runtime: Runtime::Mock,
        };
        let hub = Hub::open(config).unwrap().with_auto_draft(auto);
        hub.validate_startup().unwrap();
        let worker = hub.clone();
        let dispatcher = tokio::spawn(async move { worker.run_auto_drafts().await });
        Self {
            app: app(hub),
            dir,
            dispatcher,
        }
    }
    fn draft_turns(&self) -> i64 {
        rusqlite::Connection::open(self.dir.join("hub.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM draft_turns", [], |r| r.get(0))
            .unwrap()
    }
    async fn call(&self, who: &str, method: &str, path: &str, data: Value) -> (StatusCode, Value) {
        let r = self
            .app
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
    async fn task(&self, who: &str, id: &str) -> Value {
        let (_, tasks) = self.call(who, "GET", "/api/tasks", Value::Null).await;
        tasks
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == id)
            .unwrap()
            .clone()
    }
    async fn delegated(&self) -> String {
        let (_, p) = self
            .call(
                "alice",
                "POST",
                "/api/proposals",
                json!({"owner":"bob","summary":"做个自我介绍","request_id":"p1"}),
            )
            .await;
        let path = format!("/api/proposals/{}/actions", p["id"].as_str().unwrap());
        let (status, out) = self
            .call(
                "alice",
                "POST",
                &path,
                json!({"action":"confirm","expected_revision":0}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out["task_id"].as_str().unwrap().to_string()
    }
    async fn act(&self, who: &str, task: &str, body: Value) -> Value {
        let (status, out) = self
            .call(who, "POST", &format!("/api/tasks/{task}/actions"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out
    }
    /// The owner's view of the current version's draft once it settles.
    async fn settled(&self, task: &str) -> Value {
        for _ in 0..200 {
            let draft = self.task("bob", task).await["my_draft"].clone();
            if !matches!(draft["status"].as_str(), Some("pending" | "queued")) && !draft.is_null() {
                return draft;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("automatic draft did not settle");
    }
}

#[tokio::test]
async fn accept_starts_one_private_draft_per_task_version() {
    let f = Fixture::new(true);
    let task = f.delegated().await;
    assert!(
        f.task("bob", &task).await["my_draft"].is_null(),
        "nothing is drafted before the owner accepts"
    );

    let accepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":0}),
        )
        .await;
    let request_id = accepted["auto_draft"]["request_id"].as_str().unwrap();
    let draft = f.settled(&task).await;
    assert_eq!(draft["status"], "complete");
    assert_eq!(draft["automatic"], true);
    assert_eq!(draft["request_id"], request_id);

    // The owner reads it through the ordinary private draft receipt.
    let (status, receipt) = f
        .call(
            "bob",
            "GET",
            &format!("/api/tasks/{task}/drafts/turns/{request_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{receipt}");
    assert!(receipt["response"]["draft"]["content"].is_string());

    // Never shared, never delivered: the requester sees no draft state, the task
    // stays with the owner, and the requester cannot read the receipt.
    let alice = f.task("alice", &task).await;
    assert!(alice.get("my_draft").is_none());
    assert_eq!(alice["status"], "working");
    assert!(alice["result"].is_null());
    let (status, _) = f
        .call(
            "alice",
            "GET",
            &format!("/api/tasks/{task}/drafts/turns/{request_id}"),
            Value::Null,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(f.draft_turns(), 1, "one task version, one automatic draft");

    // A manual redraft of the same version still works alongside it.
    let (status, _) = f
        .call(
            "bob",
            "POST",
            &format!("/api/tasks/{task}/drafts"),
            json!({"request_id":"manual-1","expected_revision":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(f.task("bob", &task).await["my_draft"]["automatic"], false);

    f.act(
        "bob",
        &task,
        json!({"action":"deliver","expected_revision":1,"result":"人工审阅后的自我介绍"}),
    )
    .await;
    assert!(
        f.task("bob", &task).await["my_draft"].is_null(),
        "no draft once the task left working"
    );
    // Returned work goes back to the owner, who re-accepts; that version is
    // drafted again automatically.
    let returned = f
        .act(
            "alice",
            &task,
            json!({"action":"return","expected_revision":2}),
        )
        .await;
    assert!(returned.get("auto_draft").is_none());
    let reaccepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":3}),
        )
        .await;
    assert!(reaccepted["auto_draft"]["request_id"]
        .as_str()
        .unwrap()
        .ends_with("-r4"));
    assert_eq!(f.settled(&task).await["automatic"], true);
    assert_eq!(f.draft_turns(), 3);
}

#[tokio::test]
async fn disabled_hub_never_drafts_on_its_own() {
    let f = Fixture::new(false);
    let task = f.delegated().await;
    let accepted = f
        .act(
            "bob",
            &task,
            json!({"action":"accept","expected_revision":0}),
        )
        .await;
    assert!(accepted.get("auto_draft").is_none());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(f.draft_turns(), 0);
    assert!(f.task("bob", &task).await["my_draft"].is_null());
}

impl Fixture {
    async fn say(&self, who: &str, task: &Value, message: &str, key: &str) -> Value {
        let (status, out) = self
            .call(
                who,
                "POST",
                "/api/messages",
                json!({"message":message,"request_id":key,
                    "selected_task":{"task_id":task["id"],"expected_revision":task["revision"]}}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{out}");
        out
    }
}

#[tokio::test]
async fn plain_language_suggests_only_the_steps_the_caller_may_take() {
    let f = Fixture::new(false);
    let id = f.delegated().await;

    // Owner, awaiting: "接手" reads as accept; nothing changes until confirmed.
    let task = f.task("bob", &id).await;
    let out = f.say("bob", &task, "好的，我接手", "m1").await;
    assert_eq!(out["task_action"]["action"], "accept");
    assert_eq!(out["task_action"]["task_id"], id.as_str());
    assert_eq!(out["task_action"]["expected_revision"], 0);
    assert_eq!(f.task("bob", &id).await["status"], "awaiting_owner");
    // The requester cannot be offered the owner's step.
    let out = f.say("alice", &task, "我接手", "m2").await;
    assert!(out["task_action"].is_null());

    f.act("bob", &id, json!({"action":"accept","expected_revision":0}))
        .await;
    let task = f.task("bob", &id).await;
    let out = f
        .say("bob", &task, "我是 bob，用于面试，语气正式", "m3")
        .await;
    assert_eq!(out["task_action"]["action"], "revise");
    assert_eq!(
        out["task_action"]["instruction"],
        "我是 bob，用于面试，语气正式"
    );
    let out = f.say("bob", &task, "可以了，交付吧", "m4").await;
    assert_eq!(out["task_action"]["action"], "deliver");
    assert!(out["task_action"]["instruction"].is_null());
    assert_eq!(f.task("alice", &id).await["status"], "working");
    // The same receipt replays the same suggestion.
    let replay = f.say("bob", &task, "可以了，交付吧", "m4").await;
    assert_eq!(replay["task_action"], out["task_action"]);
    // The conversation itself is ordinary private history.
    let (_, messages) = f.call("bob", "GET", "/api/messages", Value::Null).await;
    assert!(messages
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"] == "可以了，交付吧"));

    f.act(
        "bob",
        &id,
        json!({"action":"deliver","expected_revision":1,"result":"交付内容"}),
    )
    .await;
    let task = f.task("alice", &id).await;
    assert_eq!(
        f.say("alice", &task, "验收通过", "m5").await["task_action"]["action"],
        "approve"
    );
    assert_eq!(
        f.say("alice", &task, "退回，再补充一段经历", "m6").await["task_action"]["action"],
        "return"
    );
    f.act(
        "alice",
        &id,
        json!({"action":"approve","expected_revision":2}),
    )
    .await;
    // A step that no longer fits is not offered, and the Hub says why instead
    // of leaving the model's "confirm below" with nothing to confirm.
    let done = f.task("alice", &id).await;
    let out = f.say("alice", &done, "还是退回吧", "m8").await;
    assert!(out["task_action"].is_null());
    let reply = out["reply"].as_str().unwrap();
    assert!(reply.starts_with("[系统提示] 这一步现在做不了"), "{reply}");
    assert!(reply.contains("已完成"), "{reply}");
    // Without a selected task there is never a task action.
    let (_, out) = f
        .call(
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"验收通过","request_id":"m7"}),
        )
        .await;
    assert!(out["task_action"].is_null());
}

#[tokio::test]
async fn owner_can_disable_automatic_preparation_without_disabling_manual_drafts() {
    let f = Fixture::new(true);
    let body = json!({"expected_revision":0,
        "public":{"team":"","responsibilities":"","help_with":"","request_requirements":""},
        "private":{"work_context":"","response_preferences":""},
        "permissions":{"auto_reply_public":false,"auto_prepare_drafts":false}});
    assert_eq!(
        f.call("bob", "PUT", "/api/collaboration-profile", body)
            .await
            .0,
        StatusCode::OK
    );
    let task = f.delegated().await;
    f.act(
        "bob",
        &task,
        json!({"action":"accept","expected_revision":0}),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(f.draft_turns(), 0);
    let path = format!("/api/tasks/{task}/drafts");
    let (status, _) = f
        .call(
            "bob",
            "POST",
            &path,
            json!({"request_id":format!("auto-{task}-r1"),"expected_revision":1}),
        )
        .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "auto-prefixed dispatch respects current preference too"
    );
    assert_eq!(f.draft_turns(), 0);
    let (status, draft) = f
        .call(
            "bob",
            "POST",
            &path,
            json!({"request_id":"manual-after-opt-out","expected_revision":1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(draft["status"], "complete");
    assert_eq!(f.draft_turns(), 1);
}

#[tokio::test]
async fn intent_storage_failure_rolls_back_acceptance_and_audit() {
    let f = Fixture::new(true);
    let id = f.delegated().await;
    let db = rusqlite::Connection::open(f.dir.join("hub.db")).unwrap();
    db.execute_batch("CREATE TRIGGER fixture_reject_intent BEFORE INSERT ON auto_draft_intents BEGIN SELECT RAISE(ABORT,'fixture failure'); END;").unwrap();
    let before: i64 = db
        .query_row("SELECT count(*) FROM events WHERE task_id=?1", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    let (status, _) = f
        .call(
            "bob",
            "POST",
            &format!("/api/tasks/{id}/actions"),
            json!({"action":"accept","expected_revision":0}),
        )
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let task = f.task("bob", &id).await;
    assert_eq!(task["status"], "awaiting_owner");
    assert_eq!(task["revision"], 0);
    assert_eq!(
        db.query_row("SELECT count(*) FROM events WHERE task_id=?1", [&id], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        before
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM auto_draft_intents", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(f.draft_turns(), 0);
}
