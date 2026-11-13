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

fn path(p: &str, item: &Value) -> String {
    format!(
        "/api/projects/{p}/state/{}/questions",
        item["id"].as_str().unwrap()
    )
}
async fn post(
    h: &Hub,
    p: &str,
    item: &Value,
    who: &str,
    key: &str,
    parent: Value,
) -> (StatusCode, Value) {
    call(h,who,"POST",&path(p,item),json!({"request_id":key,"expected_revision":revision(h,p).await,"expected_version":item["version"],"content":format!("feedback {key}"),"reply_to":parent})).await
}
#[tokio::test]
async fn three_people_publish_feedback_without_confirmation_or_private_context() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    join(&h, &p, "carol").await;
    let item = create_item(&h, &p, "alice", "goal").await;
    let (s, q) = post(&h, &p, &item, "alice", "q", Value::Null).await;
    assert_eq!(s, StatusCode::OK);
    let qid = q["message"]["id"].clone();
    for who in ["bob", "carol"] {
        assert_eq!(
            post(&h, &p, &item, who, who, qid.clone()).await.0,
            StatusCode::OK
        );
    }
    let list = call(&h, "carol", "GET", &path(&p, &item), Value::Null).await;
    assert_eq!(list.0, StatusCode::OK);
    assert_eq!(list.1["messages"].as_array().unwrap().len(), 3);
    assert_eq!(
        call(
            &h,
            "alice",
            "GET",
            &format!("/api/projects/{p}/state"),
            Value::Null
        )
        .await
        .1["items"][0]["status"],
        "proposed"
    );
    assert_eq!(
        call(&h, "eve", "GET", &path(&p, &item), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(&h, &p, &item, "eve", "e", qid).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.db()
            .unwrap()
            .query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[tokio::test]
async fn idempotency_is_bound_to_payload_and_current_authorization() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let item = create_item(&h, &p, "alice", "goal").await;
    let req = json!({"request_id":"q","expected_revision":revision(&h,&p).await,"expected_version":1,"content":"question","reply_to":null});
    let first = call(&h, "bob", "POST", &path(&p, &item), req.clone()).await;
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), req.clone())
            .await
            .1["message"]["id"],
        first.1["message"]["id"]
    );
    let mut changed = req.clone();
    changed["content"] = json!("different");
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), changed).await.0,
        StatusCode::CONFLICT
    );
    let other = create_item(&h, &p, "alice", "decision").await;
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &other), req.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/members/bob/remove"),
            json!({"expected_revision":revision(&h,&p).await})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), req).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(&h, "bob", "GET", &path(&p, &item), Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn withdrawal_is_author_only_redacted_and_blocks_replies_not_existing_history() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let item = create_item(&h, &p, "alice", "goal").await;
    let q = post(&h, &p, &item, "alice", "q", Value::Null).await.1;
    let r = post(&h, &p, &item, "bob", "r", q["message"]["id"].clone())
        .await
        .1;
    let url = format!(
        "{}/{}/withdraw",
        path(&p, &item),
        q["message"]["id"].as_str().unwrap()
    );
    let req = json!({"expected_revision":revision(&h,&p).await});
    assert_eq!(
        call(&h, "bob", "POST", &url, req.clone()).await.0,
        StatusCode::NOT_FOUND
    );
    let w = call(&h, "alice", "POST", &url, req.clone()).await;
    assert_eq!(w.0, StatusCode::OK);
    assert_eq!(w.1["message"]["content"], "");
    assert_eq!(call(&h, "alice", "POST", &url, req).await.0, StatusCode::OK);
    assert_eq!(
        post(&h, &p, &item, "bob", "r2", q["message"]["id"].clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let replay = post(&h, &p, &item, "alice", "q", Value::Null).await;
    assert_eq!(replay.0, StatusCode::OK);
    assert_eq!(replay.1["message"]["content"], "");
    let list = call(&h, "bob", "GET", &path(&p, &item), Value::Null)
        .await
        .1;
    assert_eq!(list["messages"][0]["content"], "");
    assert_eq!(list["messages"][1]["content"], r["message"]["content"]);
    let db = h.db().unwrap();
    assert!(db.execute("DELETE FROM project_questions", []).is_err());
    assert!(db
        .execute("UPDATE project_question_withdrawals SET author='bob'", [])
        .is_err());
}
#[tokio::test]
async fn stale_sources_archives_and_nested_or_cross_item_replies_are_rejected() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let item = create_item(&h, &p, "alice", "goal").await;
    let q = post(&h, &p, &item, "alice", "q", Value::Null).await.1;
    let r = post(&h, &p, &item, "bob", "r", q["message"]["id"].clone())
        .await
        .1;
    assert_eq!(
        post(&h, &p, &item, "bob", "nested", r["message"]["id"].clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let other = create_item(&h, &p, "alice", "decision").await;
    assert_eq!(
        post(&h, &p, &other, "bob", "cross", q["message"]["id"].clone())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        act(&h, &p, "alice", &item, "confirm").await.0,
        StatusCode::OK
    );
    assert_eq!(
        post(&h, &p, &item, "bob", "stale", Value::Null).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &format!("/api/projects/{p}/archive"),
            json!({"expected_revision":revision(&h,&p).await})
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        post(&h, &p, &other, "bob", "archive", Value::Null).await.0,
        StatusCode::CONFLICT
    );
    let url = format!(
        "{}/{}/withdraw",
        path(&p, &item),
        q["message"]["id"].as_str().unwrap()
    );
    assert_eq!(
        call(
            &h,
            "alice",
            "POST",
            &url,
            json!({"expected_revision":revision(&h,&p).await})
        )
        .await
        .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn revoked_sources_revision_conflicts_and_unknown_fields_never_publish() {
    let h = fixture();
    let p = project(&h, "p").await;
    join(&h, &p, "bob").await;
    let item = create_item(&h, &p, "alice", "goal").await;
    let mut request = json!({"request_id":"q", "expected_revision":0,"expected_version":1,"content":"question","reply_to":null});
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), request.clone())
            .await
            .0,
        StatusCode::CONFLICT
    );
    request["expected_revision"] = json!(revision(&h, &p).await);
    request["confirmed_by"] = json!("alice");
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), request.clone())
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    request.as_object_mut().unwrap().remove("confirmed_by");
    let q = call(&h, "bob", "POST", &path(&p, &item), request.clone())
        .await
        .1;
    assert_eq!(
        act(&h, &p, "alice", &item, "withdraw").await.0,
        StatusCode::OK
    );
    assert_eq!(
        post(&h, &p, &item, "bob", "new", Value::Null).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), request.clone())
            .await
            .1["message"]["id"],
        q["message"]["id"]
    );
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('bob','Bob','disabled')",[]).unwrap();
    assert_eq!(
        call(&h, "bob", "POST", &path(&p, &item), request).await.0,
        StatusCode::UNAUTHORIZED
    );
}
