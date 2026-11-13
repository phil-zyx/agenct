use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn fixture() -> Hub {
    Hub::open(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: ":memory:".into(),
        runtime: Runtime::Mock,
        users: ["alice", "bob", "carol", "eve"]
            .into_iter()
            .map(|id| UserConfig {
                id: id.into(),
                token_sha256: format!("{:x}", Sha256::digest(id.repeat(40).as_bytes())),
            })
            .collect(),
    })
    .unwrap()
}
async fn call(h: &Hub, who: &str, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app(h.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("authorization", format!("Bearer {}", who.repeat(40)))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
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
async fn project(h: &Hub, key: &str) -> String {
    let (s, v) = call(
        h,
        "alice",
        "POST",
        "/api/projects",
        json!({"name":"Launch","goal":"Shared goal","kind":"temporary","request_id":key}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["id"].as_str().unwrap().into()
}
async fn revision(h: &Hub, id: &str) -> i64 {
    let (s, v) = call(
        h,
        "alice",
        "GET",
        &format!("/api/projects/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["project"]["revision"].as_i64().unwrap()
}
async fn join(h: &Hub, id: &str, who: &str) {
    let n = revision(h, id).await;
    assert_eq!(
        call(
            h,
            "alice",
            "POST",
            &format!("/api/projects/{id}/members"),
            json!({"owner":who,"expected_revision":n})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            h,
            who,
            "POST",
            &format!("/api/projects/{id}/membership"),
            json!({"action":"accept","expected_revision":n+1})
        )
        .await
        .0,
        StatusCode::OK
    );
}
fn content(kind: &str) -> Value {
    json!({"kind":kind,"content":"Explicitly shared checkpoint","assignee":if kind=="assignment" {json!("bob")} else {Value::Null},"source_kind":"model_suggestion","source_note":"Suggestion manually published by the author, not verified evidence"})
}
async fn create_item(h: &Hub, id: &str, who: &str, kind: &str) -> Value {
    let n = revision(h, id).await;
    let (s, v) = call(
        h,
        who,
        "POST",
        &format!("/api/projects/{id}/state"),
        json!({"request_id":kind,"expected_revision":n,"content":content(kind)}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["item"].clone()
}
async fn act(h: &Hub, id: &str, who: &str, item: &Value, action: &str) -> (StatusCode, Value) {
    call(h,who,"POST",&format!("/api/projects/{id}/state/{}/actions",item["id"].as_str().unwrap()),
        json!({"action":action,"expected_revision":revision(h,id).await,"expected_version":item["version"]})).await
}

#[tokio::test]
async fn explicit_sources_start_proposed_and_do_not_copy_private_context() {
    let h = fixture();
    let id = project(&h, "project").await;
    join(&h, &id, "bob").await;
    join(&h, &id, "carol").await;
    h.db().unwrap().execute_batch("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('private-task','alice','bob','PRIVATE SUMMARY','working',1);
        INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision) VALUES ('private-draft','private-task',1,'bob','PRIVATE DRAFT','draft',1);
        INSERT INTO messages(owner,role,content,status) VALUES ('alice','user','PRIVATE CHAT','complete');").unwrap();
    let path = format!("/api/projects/{id}/state");
    assert_eq!(
        call(&h, "alice", "GET", &path, Value::Null).await.1["items"],
        json!([])
    );
    for kind in ["goal", "assignment", "blocker", "decision"] {
        let item = create_item(&h, &id, "alice", kind).await;
        assert_eq!(item["status"], "proposed");
        assert_eq!(item["version"], 1);
        assert_eq!(item["source_kind"], "model_suggestion");
        assert_eq!(item["confirmed_by"], Value::Null);
        assert_eq!(item["confirmation_valid"], false);
    }
    let (s, v) = call(&h, "carol", "GET", &path, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["items"].as_array().unwrap().len(), 4);
    for secret in ["PRIVATE", "private-task", "private-draft"] {
        assert!(!v.to_string().contains(secret));
    }
    for table in ["worker_jobs", "contact_messages", "proposals"] {
        assert_eq!(
            h.db()
                .unwrap()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        call(&h, "eve", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &h,
            "eve",
            "POST",
            &path,
            json!({"request_id":"no","expected_revision":8,"content":content("goal")})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Project membership still grants no private task or draft access.
    assert_eq!(
        call(&h, "carol", "GET", "/api/tasks/private-task", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn named_confirmations_bind_exact_versions_and_revision_clears_them() {
    let h = fixture();
    let id = project(&h, "project").await;
    join(&h, &id, "bob").await;
    join(&h, &id, "carol").await;
    for (kind, publisher, confirmer) in [
        ("goal", "bob", "alice"),
        ("decision", "bob", "alice"),
        ("blocker", "bob", "bob"),
        ("assignment", "alice", "bob"),
    ] {
        let item = create_item(&h, &id, publisher, kind).await;
        assert_eq!(
            act(&h, &id, "carol", &item, "confirm").await.0,
            StatusCode::NOT_FOUND
        );
        if kind == "assignment" {
            assert_eq!(
                act(&h, &id, "alice", &item, "confirm").await.0,
                StatusCode::NOT_FOUND
            );
        }
        let (s, v) = act(&h, &id, confirmer, &item, "confirm").await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let confirmed = v["item"].clone();
        assert_eq!(confirmed["version"], 2);
        assert_eq!(confirmed["confirmed_by"], confirmer);
        assert_eq!(confirmed["confirmation_valid"], true);
        assert_eq!(confirmed["source_kind"], "model_suggestion");
        assert_eq!(
            act(&h, &id, confirmer, &item, "confirm").await.0,
            StatusCode::CONFLICT
        );
        let path = format!(
            "/api/projects/{id}/state/{}/actions",
            item["id"].as_str().unwrap()
        );
        let mut new_content = content(kind);
        new_content["content"] = json!("Changed explicitly shared checkpoint");
        let (s,v) = call(&h,publisher,"POST",&path,json!({"action":"revise","expected_revision":revision(&h,&id).await,"expected_version":2,"content":new_content})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["item"]["version"], 3);
        assert_eq!(v["item"]["status"], "proposed");
        assert_eq!(v["item"]["confirmed_by"], Value::Null);
        assert_eq!(
            act(&h, &id, confirmer, &confirmed, "confirm").await.0,
            StatusCode::CONFLICT
        );
        let history = format!("/api/projects/{id}/state/{}", item["id"].as_str().unwrap());
        let (_, v) = call(&h, "carol", "GET", &history, Value::Null).await;
        assert_eq!(v["versions"].as_array().unwrap().len(), 3);
        assert_eq!(v["versions"][0]["content"], "Explicitly shared checkpoint");
        assert_eq!(v["versions"][1]["confirmed_by"], confirmer);
        assert_eq!(v["versions"][1]["confirmation_valid"], false);
    }
}

#[tokio::test]
async fn replay_and_concurrent_actions_cannot_overwrite_or_reconfirm() {
    let h = fixture();
    let id = project(&h, "project").await;
    let item = create_item(&h, &id, "alice", "goal").await;
    let path = format!("/api/projects/{id}/state");
    let payload = json!({"request_id":"goal","expected_revision":0,"content":content("goal")});
    let (s, v) = call(&h, "alice", "POST", &path, payload.clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["item"]["id"], item["id"]);
    assert_eq!(revision(&h, &id).await, 1);
    let mut different = payload;
    different["content"]["source_note"] = json!("Different provenance");
    assert_eq!(
        call(&h, "alice", "POST", &path, different).await.0,
        StatusCode::CONFLICT
    );
    let action_path = format!("{path}/{}/actions", item["id"].as_str().unwrap());
    let confirm = json!({"action":"confirm","expected_revision":1,"expected_version":1});
    let (a, b) = tokio::join!(
        call(&h, "alice", "POST", &action_path, confirm.clone()),
        call(&h, "alice", "POST", &action_path, confirm)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert_eq!(revision(&h, &id).await, 2);
    let (s, v) = call(
        &h,
        "alice",
        "POST",
        &path,
        json!({"request_id":"goal","expected_revision":0,"content":content("goal")}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["item"]["version"], 2);
    let other = project(&h, "other-project").await;
    assert_eq!(
        call(
            &h,
            "alice",
            "GET",
            &format!(
                "/api/projects/{other}/state/{}",
                item["id"].as_str().unwrap()
            ),
            Value::Null
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn removed_or_departed_members_lose_access_and_rejoin_does_not_restore_commitments() {
    for leave in [true, false] {
        let h = fixture();
        let id = project(&h, "project").await;
        join(&h, &id, "bob").await;
        join(&h, &id, "carol").await;
        let item = create_item(&h, &id, "alice", "assignment").await;
        let (_, v) = act(&h, &id, "bob", &item, "confirm").await;
        assert_eq!(v["item"]["status"], "confirmed");
        let n = revision(&h, &id).await;
        let (s, _) = if leave {
            call(
                &h,
                "bob",
                "POST",
                &format!("/api/projects/{id}/membership"),
                json!({"action":"leave","expected_revision":n}),
            )
            .await
        } else {
            call(
                &h,
                "alice",
                "POST",
                &format!("/api/projects/{id}/members/bob/remove"),
                json!({"expected_revision":n}),
            )
            .await
        };
        assert_eq!(s, StatusCode::OK);
        let path = format!("/api/projects/{id}/state/{}", item["id"].as_str().unwrap());
        assert_eq!(
            call(&h, "bob", "GET", &path, Value::Null).await.0,
            StatusCode::NOT_FOUND
        );
        let (_, v) = call(&h, "carol", "GET", &path, Value::Null).await;
        assert_eq!(v["item"]["status"], "proposed");
        assert_eq!(v["item"]["change"], "membership_revoked");
        assert_eq!(v["item"]["changed_by"], if leave { "bob" } else { "alice" });
        assert_eq!(v["item"]["version"], 3);
        join(&h, &id, "bob").await;
        let (_, v) = call(&h, "bob", "GET", &path, Value::Null).await;
        assert_eq!(v["item"]["status"], "proposed");
        assert_eq!(v["versions"][1]["confirmed_by"], "bob");
        assert_eq!(
            act(&h, &id, "bob", &v["item"], "confirm").await.0,
            StatusCode::OK
        );
    }
}

#[tokio::test]
async fn invitation_archive_disabled_account_and_tenant_checks_cover_every_route() {
    let mut h = fixture();
    let id = project(&h, "project").await;
    join(&h, &id, "bob").await;
    let item = create_item(&h, &id, "alice", "assignment").await;
    assert_eq!(
        act(&h, &id, "bob", &item, "confirm").await.0,
        StatusCode::OK
    );
    let n = revision(&h, &id).await;
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{id}/members"),
            json!({"owner":"carol","expected_revision":n})
        )
        .await
        .0,
        StatusCode::OK
    );
    let path = format!("/api/projects/{id}/state");
    assert_eq!(
        call(&h, "carol", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        act(&h, &id, "carol", &item, "confirm").await.0,
        StatusCode::NOT_FOUND
    );
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('bob','Bob','disabled')",[]).unwrap();
    assert_eq!(
        call(&h, "bob", "GET", &path, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
    let (_, v) = call(&h, "alice", "GET", &path, Value::Null).await;
    assert_eq!(v["items"][0]["confirmation_valid"], false);
    h.db()
        .unwrap()
        .execute(
            "UPDATE member_profiles SET status='active' WHERE owner='bob'",
            [],
        )
        .unwrap();
    h.directory = Arc::new(
        directory::Directory::new(["alice", "bob", "carol", "eve"].map(String::from))
            .unwrap()
            .with_tenants(
                [
                    ("alice", "one"),
                    ("bob", "two"),
                    ("carol", "one"),
                    ("eve", "two"),
                ]
                .into_iter()
                .map(|(u, t)| (u.into(), t.into()))
                .collect(),
            )
            .unwrap(),
    );
    assert_eq!(
        call(&h, "bob", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        act(&h, &id, "bob", &item, "confirm").await.0,
        StatusCode::NOT_FOUND
    );
    let n = revision(&h, &id).await;
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{id}/archive"),
            json!({"expected_revision":n})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &path,
            json!({"request_id":"new","expected_revision":n+1,"content":content("goal")})
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&h, "alice", "GET", &path, Value::Null).await.0,
        StatusCode::OK
    );
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('alice','Alice','disabled')",[]).unwrap();
    assert_eq!(
        call(&h, "alice", "GET", &path, Value::Null).await.0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn validation_and_append_only_history_reject_forgery() {
    let h = fixture();
    let id = project(&h, "project").await;
    let path = format!("/api/projects/{id}/state");
    let base = json!({"request_id":"x","expected_revision":0,"content":content("goal")});
    for (field, value) in [
        ("status", json!("confirmed")),
        ("confirmed_by", json!("bob")),
        ("source_message_id", json!(17)),
    ] {
        let mut body = base.clone();
        body["content"][field] = value;
        assert_eq!(
            call(&h, "alice", "POST", &path, body).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    for (field, value) in [
        ("kind", json!("invented")),
        ("content", json!(" ")),
        ("source_note", json!("")),
        ("source_kind", json!("verified_fact")),
        ("assignee", json!("bob")),
    ] {
        let mut body = base.clone();
        body["content"][field] = value;
        assert_eq!(
            call(&h, "alice", "POST", &path, body).await.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(revision(&h, &id).await, 0);
    let item = create_item(&h, &id, "alice", "goal").await;
    let (_, v) = act(&h, &id, "alice", &item, "withdraw").await;
    assert_eq!(v["item"]["status"], "withdrawn");
    assert_eq!(
        act(&h, &id, "alice", &v["item"], "confirm").await.0,
        StatusCode::CONFLICT
    );
    let db = h.db().unwrap();
    for sql in [
        "UPDATE project_state_versions SET content='forged'",
        "DELETE FROM project_state_versions",
        "UPDATE project_state_items SET created_by='bob'",
        "DELETE FROM project_state_items",
        "INSERT OR REPLACE INTO project_state_versions SELECT * FROM project_state_versions",
        "INSERT OR REPLACE INTO project_state_items SELECT * FROM project_state_items",
    ] {
        assert!(db.execute_batch(sql).is_err(), "{sql}");
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM project_state_versions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn exact_creation_replay_remains_read_only_after_departure_and_archive() {
    let h = fixture();
    let id = project(&h, "project").await;
    join(&h, &id, "bob").await;
    let item = create_item(&h, &id, "alice", "assignment").await;
    let (_, v) = act(&h, &id, "bob", &item, "confirm").await;
    assert_eq!(v["item"]["status"], "confirmed");
    let n = revision(&h, &id).await;
    assert_eq!(
        call(
            &h,
            "bob",
            "POST",
            &format!("/api/projects/{id}/membership"),
            json!({"action":"leave","expected_revision":n})
        )
        .await
        .0,
        StatusCode::OK
    );
    let path = format!("/api/projects/{id}/state");
    let replay =
        json!({"request_id":"assignment","expected_revision":2,"content":content("assignment")});
    let (s, v) = call(&h, "alice", "POST", &path, replay.clone()).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["item"]["id"], item["id"]);
    assert_eq!(v["item"]["version"], 3);
    assert_eq!(v["item"]["status"], "proposed");
    let n = revision(&h, &id).await;
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{id}/archive"),
            json!({"expected_revision":n})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, "alice", "POST", &path, replay).await.0,
        StatusCode::OK
    );
    assert_eq!(revision(&h, &id).await, n + 1);
}

#[tokio::test]
async fn withdrawal_still_invalidates_confirmation_when_creator_is_disabled_or_project_archived() {
    for disabled in [true, false] {
        let h = fixture();
        let id = project(&h, "project").await;
        join(&h, &id, "bob").await;
        let item = create_item(&h, &id, "alice", "assignment").await;
        assert_eq!(
            act(&h, &id, "bob", &item, "confirm").await.0,
            StatusCode::OK
        );
        let n = revision(&h, &id).await;
        let leave_revision = if disabled {
            h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('alice','Alice','disabled')",[]).unwrap();
            n
        } else {
            assert_eq!(
                call(
                    &h,
                    "alice",
                    "POST",
                    &format!("/api/projects/{id}/archive"),
                    json!({"expected_revision":n})
                )
                .await
                .0,
                StatusCode::OK
            );
            n + 1
        };
        assert_eq!(
            call(
                &h,
                "bob",
                "POST",
                &format!("/api/projects/{id}/membership"),
                json!({"action":"leave","expected_revision":leave_revision})
            )
            .await
            .0,
            StatusCode::OK
        );
        let db = h.db().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT status FROM project_state_versions ORDER BY version DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "proposed"
        );
        assert_eq!(
            db.query_row(
                "SELECT change FROM project_state_versions ORDER BY version DESC LIMIT 1",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "membership_revoked"
        );
    }
}

#[tokio::test]
async fn failed_confirmation_revocation_rolls_back_membership_revision_and_all_versions() {
    let h = fixture();
    let id = project(&h, "project").await;
    join(&h, &id, "bob").await;
    let item = create_item(&h, &id, "alice", "assignment").await;
    assert_eq!(
        act(&h, &id, "bob", &item, "confirm").await.0,
        StatusCode::OK
    );
    let n = revision(&h, &id).await;
    h.db().unwrap().execute_batch("CREATE TRIGGER fail_project_state_revocation BEFORE INSERT ON project_state_versions WHEN NEW.change='membership_revoked' BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let (s, _) = call(
        &h,
        "bob",
        "POST",
        &format!("/api/projects/{id}/membership"),
        json!({"action":"leave","expected_revision":n}),
    )
    .await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(revision(&h, &id).await, n);
    let path = format!("/api/projects/{id}/state/{}", item["id"].as_str().unwrap());
    let (s, v) = call(&h, "bob", "GET", &path, Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["item"]["status"], "confirmed");
    assert_eq!(v["item"]["version"], 2);
    assert_eq!(v["versions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn project_discussion_is_private_bound_and_revocable() {
    let h = fixture();
    let id = project(&h, "discussion").await;
    join(&h, &id, "bob").await;
    let item = create_item(&h, &id, "alice", "goal").await;
    let rev = revision(&h, &id).await;
    let selection = json!({"project_id":id,"expected_revision":rev});
    let request = json!({"message":"What should we align next?", "request_id":"discussion-turn", "selected_project":selection});
    let (s, response) = call(&h, "bob", "POST", "/api/messages", request.clone()).await;
    assert_eq!(s, StatusCode::OK, "{response}");
    assert_eq!(response["project_context"]["project_id"], id);
    assert_eq!(response["project_context"]["items"][0]["id"], item["id"]);
    assert!(response["proposal"].is_null());
    assert!(response["task_action"].is_null());
    assert_eq!(revision(&h, &id).await, rev);
    assert_eq!(
        call(&h, "bob", "POST", "/api/messages", request.clone())
            .await
            .1,
        response
    );
    let mut changed = request.clone();
    changed["selected_project"]["expected_revision"] = json!(rev + 1);
    assert_eq!(
        call(&h, "bob", "POST", "/api/messages", changed).await.0,
        StatusCode::CONFLICT
    );
    let mut other = request.clone();
    other["request_id"] = json!("outsider");
    assert_eq!(
        call(&h, "eve", "POST", "/api/messages", other).await.0,
        StatusCode::NOT_FOUND
    );
    {
        let db = h.db().unwrap();
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM messages WHERE owner='bob' AND project_id IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            2
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM messages WHERE owner='bob' AND project_id IS NULL",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM proposals", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        call(
            &h,
            "bob",
            "POST",
            &format!("/api/projects/{id}/membership"),
            json!({"action":"leave","expected_revision":rev})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, "bob", "POST", "/api/messages", request.clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&h, "bob", "GET", "/api/turns/discussion-turn", Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let mut fresh = request;
    fresh["request_id"] = json!("after-leave");
    assert_eq!(
        call(&h, "bob", "POST", "/api/messages", fresh).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn project_context_bounds_whole_items_and_rejects_mixed_selection() {
    let h = fixture();
    let id = project(&h, "bounded-discussion").await;
    for n in 0..6 {
        let rev = revision(&h, &id).await;
        let mut c = content("goal");
        c["content"] = json!("界".repeat(4000));
        assert_eq!(
            call(
                &h,
                "alice",
                "POST",
                &format!("/api/projects/{id}/state"),
                json!({"request_id":format!("large-{n}"),"expected_revision":rev,"content":c})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    let rev = revision(&h, &id).await;
    let selection = crate::project_context::Selection {
        project_id: id.clone(),
        expected_revision: rev,
    };
    let context = crate::project_context::capture(&h, &h.db().unwrap(), "alice", Some(&selection))
        .unwrap()
        .unwrap();
    assert!(context.to_string().len() <= crate::project_context::MAX_BYTES);
    assert!(context["omitted_items"].as_u64().unwrap() > 0);
    assert_eq!(context["items"][0]["content"], "界".repeat(4000));
    let body = json!({"message":"hello","request_id":"mixed","selected_project":selection,"selected_task":{"task_id":"fake","expected_revision":0}});
    assert_eq!(
        call(&h, "alice", "POST", "/api/messages", body).await.0,
        StatusCode::BAD_REQUEST
    );
    let stale = json!({"message":"hello","request_id":"stale-project","selected_project":{"project_id":id,"expected_revision":rev-1}});
    assert_eq!(
        call(&h, "alice", "POST", "/api/messages", stale).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn project_discussion_revoked_during_adapter_call_never_commits_or_redispatches() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let (e, r, c) = (entered.clone(), release.clone(), calls.clone());
    let adapter=Router::new().route("/v1/turn",post(move |Json(payload):Json<Value>| {
        let (e,r,c)=(e.clone(),r.clone(),c.clone());
        async move {
            c.fetch_add(1,Ordering::SeqCst);
            assert_eq!(payload["history"].as_array().unwrap().len(),1);
            assert!(!payload.to_string().contains("PRIVATE_SENTINEL"));
            e.notify_one(); r.notified().await;
            Json(json!({"reply":"REVOKED_REPLY_SENTINEL","backend":"dsh","proposal":null,"profile_update":null}))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
    let mut h = fixture();
    h.runtime = Runtime::Dsh {
        url,
        token: "fake-adapter-token".into(),
    };
    let id = project(&h, "revoke-running").await;
    join(&h, &id, "bob").await;
    {
        let db = h.db().unwrap();
        db.execute("INSERT INTO messages(owner,role,content,status) VALUES ('bob','user','PRIVATE_SENTINEL','complete')",[]).unwrap();
    }
    let rev = revision(&h, &id).await;
    let body = json!({"message":"What next?","request_id":"revoking-turn","selected_project":{"project_id":id,"expected_revision":rev}});
    let (cloned, request) = (h.clone(), body.clone());
    let pending =
        tokio::spawn(async move { call(&cloned, "bob", "POST", "/api/messages", request).await });
    entered.notified().await;
    assert_eq!(
        call(
            &h,
            "bob",
            "POST",
            &format!("/api/projects/{id}/membership"),
            json!({"action":"leave","expected_revision":rev})
        )
        .await
        .0,
        StatusCode::OK
    );
    release.notify_one();
    assert_eq!(pending.await.unwrap().0, StatusCode::NOT_FOUND);
    assert_eq!(
        call(&h, "bob", "POST", "/api/messages", body).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let db = h.db().unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM messages WHERE content LIKE '%REVOKED_REPLY_SENTINEL%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT status FROM chat_turns WHERE request_id='revoking-turn'",
            [],
            |r| r.get::<_, String>(0)
        )
        .unwrap(),
        "uncertain"
    );
    server.abort();
}
