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
async fn view(h: &Hub, p: &str) -> Value {
    let (s, v) = call(
        h,
        "alice",
        "GET",
        &format!("/api/projects/{p}/completion"),
        Value::Null,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v
}
async fn confirmed(h: &Hub, p: &str, who: &str, kind: &str) -> Value {
    let item = create_item(h, p, who, kind).await;
    let (s, v) = act(h, p, who, &item, "confirm").await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["item"].clone()
}
async fn resolution(
    h: &Hub,
    p: &str,
    item: &Value,
    who: &str,
    key: &str,
    action: &str,
) -> (StatusCode, Value) {
    let v = view(h, p).await;
    call(h,who,"POST",&format!("/api/projects/{p}/state/{}/resolution",item["id"].as_str().unwrap()),json!({"request_id":key,"expected_revision":v["project_revision"],"expected_snapshot":v["snapshot_token"],"expected_version":item["version"],"action":action,"note":"Human checked shared evidence"})).await
}
fn acceptance(v: &Value, key: &str) -> Value {
    json!({"request_id":key,"expected_revision":v["project_revision"],"expected_snapshot":v["snapshot_token"],"goals":v["goals"],"tasks":v["tasks"],"note":"Creator accepts this exact scope, not team consensus"})
}
#[tokio::test]
async fn explicit_resolution_reopen_and_project_acceptance_are_distinct() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    join(&h, &p, "carol").await;
    assert_eq!(view(&h, &p).await["ready"], false);
    let _goal = confirmed(&h, &p, "alice", "goal").await;
    let blocker = confirmed(&h, &p, "bob", "blocker").await;
    assert_eq!(view(&h, &p).await["ready"], false);
    assert_eq!(
        resolution(&h, &p, &blocker, "alice", "wrong", "resolve")
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "resolve", "resolve")
            .await
            .0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(v["ready"], true);
    assert!(v["acceptance"].is_null());
    let input = acceptance(&v, "accept");
    let path = format!("/api/projects/{p}/completion");
    assert_eq!(
        call(&h, "bob", "POST", &path, input.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    let (s, a) = call(&h, "alice", "POST", &path, input.clone()).await;
    assert_eq!(s, StatusCode::OK, "{a}");
    assert_eq!(a["acceptance"]["valid"], true);
    assert!(a["acceptance"].get("tasks").is_none());
    assert_eq!(
        call(&h, "alice", "POST", &path, input.clone()).await.1["replayed"],
        true
    );
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "reopen", "reopen")
            .await
            .0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(v["ready"], false);
    assert_eq!(v["acceptance"]["valid"], false);
    assert_eq!(
        call(&h, "alice", "POST", &path, input).await.1["acceptance"]["valid"],
        false
    );
    assert_eq!(
        act(&h, &p, "bob", &blocker, "withdraw").await.0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(v["blockers"][0]["resolved"], false);
    assert_eq!(v["blockers"][0]["status"], "withdrawn");
    assert_eq!(v["ready"], true);
    assert_eq!(
        call(&h, "eve", "GET", &path, Value::Null).await.0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn account_epoch_and_membership_revocation_never_resurrect_resolution() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let _goal = confirmed(&h, &p, "alice", "goal").await;
    let blocker = confirmed(&h, &p, "bob", "blocker").await;
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "r1", "resolve").await.0,
        StatusCode::OK
    );
    let old = view(&h, &p).await;
    let input = acceptance(&old, "a1");
    {
        let db = h.db().unwrap();
        db.execute("INSERT INTO member_profiles(owner,display_name,status,revision) VALUES ('bob','Bob','active',2)",[]).unwrap();
    }
    assert_eq!(view(&h, &p).await["blockers"][0]["resolved"], false);
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            input
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "r2", "resolve").await.0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            acceptance(&v, "a2")
        )
        .await
        .0,
        StatusCode::OK
    );
    {
        let db = h.db().unwrap();
        db.execute(
            "UPDATE member_profiles SET revision=revision+2 WHERE owner='bob'",
            [],
        )
        .unwrap();
    }
    assert_eq!(view(&h, &p).await["acceptance"]["valid"], false);
    assert_eq!(
        call(
            &h,
            "bob",
            "POST",
            &format!("/api/projects/{p}/membership"),
            json!({"action":"leave","expected_revision":revision(&h,&p).await})
        )
        .await
        .0,
        StatusCode::OK
    );
    join(&h, &p, "bob").await;
    let v = view(&h, &p).await;
    assert_eq!(v["blockers"][0]["resolved"], false);
    assert_eq!(v["acceptance"]["valid"], false);
}
#[tokio::test]
async fn snapshot_task_races_retraction_and_private_task_projection() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    join(&h, &p, "carol").await;
    let _goal = confirmed(&h, &p, "alice", "goal").await;
    {
        let db = h.db().unwrap();
        db.execute("INSERT INTO tasks(id,requester,owner,summary,status,result,revision) VALUES ('t','alice','bob','SECRET','completed','PRIVATE',3)",[]).unwrap();
        db.execute("INSERT INTO project_tasks(project_id,task_id,label,proposed_by,status) VALUES (?1,'t','Shared label','alice','active')",[&p]).unwrap();
    }
    let v = view(&h, &p).await;
    assert!(!v.to_string().contains("SECRET"));
    assert!(!v.to_string().contains("PRIVATE"));
    let input = acceptance(&v, "a");
    {
        h.db()
            .unwrap()
            .execute("UPDATE tasks SET revision=4 WHERE id='t'", [])
            .unwrap();
    }
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            input
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let v = view(&h, &p).await;
    let (_, a) = call(
        &h,
        "alice",
        "POST",
        &format!("/api/projects/{p}/completion"),
        acceptance(&v, "a2"),
    )
    .await;
    let aid = a["acceptance"]["id"].as_str().unwrap();
    {
        h.db()
            .unwrap()
            .execute(
                "UPDATE project_tasks SET status='revoked' WHERE task_id='t'",
                [],
            )
            .unwrap();
    }
    let v = view(&h, &p).await;
    assert_eq!(v["acceptance"]["valid"], false);
    assert_eq!(v["tasks"], json!([]));
    assert!(v["acceptance"].get("tasks").is_none());
    let path = format!("/api/projects/{p}/completion/{aid}/withdraw");
    let input = json!({"expected_revision":v["project_revision"]});
    assert_eq!(
        call(&h, "bob", "POST", &path, input.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&h, "alice", "POST", &path, input.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, "alice", "POST", &path, input).await.0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(v["acceptance"]["withdrawn"], true);
    assert_eq!(v["acceptance"]["note"], "");
}
#[tokio::test]
async fn stale_previews_request_collisions_and_immutable_records_fail_closed() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let _goal = confirmed(&h, &p, "alice", "goal").await;
    let b = confirmed(&h, &p, "bob", "blocker").await;
    let v = view(&h, &p).await;
    let path = format!(
        "/api/projects/{p}/state/{}/resolution",
        b["id"].as_str().unwrap()
    );
    let input = json!({"request_id":"resolve","expected_revision":v["project_revision"],"expected_snapshot":v["snapshot_token"],"expected_version":b["version"],"action":"resolve","note":"explicit evidence"});
    let mut wrong = input.clone();
    wrong["expected_snapshot"] = json!("stale");
    assert_eq!(
        call(&h, "bob", "POST", &path, wrong).await.0,
        StatusCode::CONFLICT
    );
    let (s, first) = call(&h, "bob", "POST", &path, input.clone()).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(first["resolution"]["valid"], true);
    assert_eq!(
        call(&h, "bob", "POST", &path, input.clone()).await.1["replayed"],
        true
    );
    let mut wrong = input.clone();
    wrong["note"] = json!("different evidence");
    assert_eq!(
        call(&h, "bob", "POST", &path, wrong).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        resolution(&h, &p, &b, "bob", "reopen", "reopen").await.0,
        StatusCode::OK
    );
    let replay = call(&h, "bob", "POST", &path, input).await.1;
    assert_eq!(replay["resolution"]["valid"], false);
    assert_eq!(
        resolution(&h, &p, &b, "bob", "again", "resolve").await.0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    let mut wrong = acceptance(&v, "accept");
    wrong["goals"][0]["item_version"] = json!(999);
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            wrong
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, a) = call(
        &h,
        "alice",
        "POST",
        &format!("/api/projects/{p}/completion"),
        acceptance(&v, "accept"),
    )
    .await;
    {
        let db = h.db().unwrap();
        for sql in [
            "UPDATE project_resolutions SET note='fake'",
            "DELETE FROM project_resolutions",
            "UPDATE project_acceptances SET note='fake'",
            "DELETE FROM project_acceptances",
        ] {
            assert!(db.execute(sql, []).is_err());
        }
    }
    assert_eq!(a["acceptance"]["valid"], true);
    {
        h.db()
            .unwrap()
            .execute(
                "UPDATE projects SET status='archived',revision=revision+1 WHERE id=?1",
                [&p],
            )
            .unwrap();
    }
    let v = view(&h, &p).await;
    assert_eq!(v["ready"], false);
    assert_eq!(v["acceptance"]["valid"], false);
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            acceptance(&v, "new")
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        resolution(&h, &p, &b, "bob", "nope", "reopen").await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn private_discussion_sees_resolution_and_acceptance_without_private_material() {
    let h = fixture();
    let p = project(&h, "context").await;
    join(&h, &p, "bob").await;
    confirmed(&h, &p, "alice", "goal").await;
    let blocker = confirmed(&h, &p, "bob", "blocker").await;
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "resolve-context", "resolve")
            .await
            .0,
        StatusCode::OK
    );
    let v = view(&h, &p).await;
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/completion"),
            acceptance(&v, "accept-context")
        )
        .await
        .0,
        StatusCode::OK
    );
    call(
        &h,
        "bob",
        "POST",
        "/api/messages",
        json!({"message":"PRIVATE-CONTEXT-SECRET","request_id":"private-context"}),
    )
    .await;
    let selection = project_context::Selection {
        project_id: p.clone(),
        expected_revision: revision(&h, &p).await,
    };
    let before = project_context::capture(&h, &h.db().unwrap(), "alice", Some(&selection))
        .unwrap()
        .unwrap();
    assert_eq!(before["project_acceptance"]["valid"], true);
    assert!(before["project_acceptance"].get("tasks").is_none());
    assert!(before.get("tasks").is_none());
    assert!(before.to_string().len() <= project_context::MAX_BYTES);
    assert!(!before.to_string().contains("PRIVATE-CONTEXT-SECRET"));
    let record = before["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == blocker["id"])
        .unwrap();
    assert_eq!(record["blocker_resolution"]["resolved"], true);
    assert_eq!(
        resolution(&h, &p, &blocker, "bob", "reopen-context", "reopen")
            .await
            .0,
        StatusCode::OK
    );
    assert!(project_context::capture(&h, &h.db().unwrap(), "alice", Some(&selection)).is_err());
    let current = project_context::Selection {
        project_id: p.clone(),
        expected_revision: revision(&h, &p).await,
    };
    let after = project_context::capture(&h, &h.db().unwrap(), "alice", Some(&current))
        .unwrap()
        .unwrap();
    assert_eq!(after["project_acceptance"]["valid"], false);
    let record = after["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == blocker["id"])
        .unwrap();
    assert_eq!(record["blocker_resolution"]["resolved"], false);
    assert_ne!(before["snapshot_sha256"], after["snapshot_sha256"]);
}
