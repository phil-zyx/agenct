use super::*;
use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

fn fixture() -> Hub {
    Hub::open(Config {
        bind: "127.0.0.1:0".parse().unwrap(), database: ":memory:".into(), runtime: Runtime::Mock,
        users: ["alice", "bob", "carol", "eve"].into_iter().map(|id| UserConfig {
            id: id.into(), token_sha256: format!("{:x}",Sha256::digest(id.repeat(40).as_bytes())),
        }).collect(),
    }).unwrap()
}
async fn call(h: &Hub, who: &str, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app(h.clone()).oneshot(Request::builder().method(method).uri(path)
        .header("authorization",format!("Bearer {}",who.repeat(40)))
        .header("content-type","application/json").body(Body::from(body.to_string())).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status,serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
async fn project(h: &Hub) -> String {
    let (status,p) = call(h,"alice","POST","/api/projects",json!({"name":"Launch","goal":"Ship together","kind":"temporary","request_id":"project-1"})).await;
    assert_eq!(status,StatusCode::OK);
    assert_eq!(p["revision"],0);
    p["id"].as_str().unwrap().into()
}
async fn revision(h: &Hub, id: &str) -> i64 {
    let (status,p) = call(h,"alice","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(status,StatusCode::OK);
    p["project"]["revision"].as_i64().unwrap()
}
async fn join(h: &Hub, id: &str, who: &str) {
    let expected = revision(h,id).await;
    let (status,body) = call(h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":who,"expected_revision":expected})).await;
    assert_eq!(status,StatusCode::OK, "{body}");
    let (status,body) = call(h,who,"POST",&format!("/api/projects/{id}/membership"),json!({"action":"accept","expected_revision":expected+1})).await;
    assert_eq!(status,StatusCode::OK, "{body}");
}
fn task(h: &Hub) {
    h.db().unwrap().execute_batch("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('private-task','alice','bob','PRIVATE SUMMARY','working',1);
        INSERT INTO task_details(task_id,author,content) VALUES ('private-task','alice','PRIVATE MATERIAL');
        INSERT INTO deliveries(id,task_id,seq,author,content,status,task_revision) VALUES ('draft','private-task',1,'bob','PRIVATE DRAFT','draft',1),('delivery','private-task',1,'bob','PRIVATE DELIVERY','submitted',1);
        INSERT INTO messages(owner,role,content,status) VALUES ('alice','user','PRIVATE CHAT','complete');
        INSERT INTO contact_messages(sender,recipient,author_kind,body) VALUES ('alice','bob','human','PRIVATE CONTACT');").unwrap();
}

#[tokio::test]
async fn creation_replay_is_bound_and_does_not_share_or_create_work() {
    let h = fixture();
    let id = project(&h).await;
    let body = json!({"name":"Launch","goal":"Ship together","kind":"temporary","request_id":"project-1"});
    let (status,p) = call(&h,"alice","POST","/api/projects",body.clone()).await;
    assert_eq!(status,StatusCode::OK);
    assert_eq!(p["id"],id);
    let mut changed = body;
    changed["goal"] = json!("different");
    assert_eq!(call(&h,"alice","POST","/api/projects",changed).await.0,StatusCode::CONFLICT);
    assert_eq!(call(&h,"bob","GET","/api/projects",Value::Null).await.1,json!([]));
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    for table in ["tasks","worker_jobs","messages","contact_messages","proposals"] {
        assert_eq!(h.db().unwrap().query_row(&format!("SELECT count(*) FROM {table}"),[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    for bad in [json!({"name":" ","goal":"goal","kind":"temporary","request_id":"x"}),
        json!({"name":"x","goal":"goal","kind":"unknown","request_id":"x"}),
        json!({"name":"x","goal":"goal","kind":"ongoing","request_id":"bad key"})] {
        assert_eq!(call(&h,"alice","POST","/api/projects",bad).await.0,StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn invitation_requires_acceptance_and_only_creator_manages_roster() {
    let h = fixture();
    let id = project(&h).await;
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":"bob","expected_revision":0})).await.0,StatusCode::OK);
    let (_,invitations) = call(&h,"bob","GET","/api/project-invitations",Value::Null).await;
    assert_eq!(invitations[0]["goal"],"Ship together");
    assert_eq!(invitations[0]["revision"],1);
    assert!(invitations[0].get("members").is_none());
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"eve","POST",&format!("/api/projects/{id}/membership"),json!({"action":"accept","expected_revision":1})).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"bob","POST",&format!("/api/projects/{id}/membership"),json!({"action":"accept","expected_revision":0})).await.0,StatusCode::CONFLICT);
    assert_eq!(call(&h,"bob","POST",&format!("/api/projects/{id}/membership"),json!({"action":"accept","expected_revision":1})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","POST",&format!("/api/projects/{id}/members"),json!({"owner":"eve","expected_revision":2})).await.0,StatusCode::NOT_FOUND);
    join(&h,&id,"carol").await;
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["members"].as_array().unwrap().len(),3);
    assert_eq!(call(&h,"eve","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/membership"),json!({"action":"leave","expected_revision":4})).await.0,StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn bilateral_task_links_share_only_projection_and_leave_revokes_consent() {
    let h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    join(&h,&id,"carol").await;
    task(&h);
    let links = format!("/api/projects/{id}/tasks");
    assert_eq!(call(&h,"carol","POST",&links,json!({"task_id":"private-task","label":"Shared checkpoint","expected_revision":4})).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"alice","POST",&links,json!({"task_id":"private-task","label":"Shared checkpoint","expected_revision":4})).await.0,StatusCode::OK);
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"],json!([]));
    let action = format!("{links}/private-task/actions");
    for who in ["alice","carol","eve"] {
        assert_eq!(call(&h,who,"POST",&action,json!({"action":"approve","expected_revision":5})).await.0,StatusCode::NOT_FOUND);
    }
    assert_eq!(call(&h,"bob","POST",&action,json!({"action":"approve","expected_revision":4})).await.0,StatusCode::CONFLICT);
    assert_eq!(call(&h,"bob","POST",&action,json!({"action":"approve","expected_revision":5})).await.0,StatusCode::OK);
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"][0]["label"],"Shared checkpoint");
    assert_eq!(p["tasks"][0]["task_status"],"working");
    assert_eq!(p["tasks"][0]["can_open_task"],false);
    assert!(!p.to_string().contains("PRIVATE"));
    assert_eq!(call(&h,"carol","GET","/api/tasks/private-task",Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"carol","POST","/api/tasks/private-task/actions",json!({"action":"cancel","reason":"no","expected_revision":1})).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"carol","GET","/api/tasks/private-task/drafts",Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"carol","GET","/api/tasks",Value::Null).await.1,json!([]));
    assert_eq!(call(&h,"bob","POST",&format!("/api/projects/{id}/membership"),json!({"action":"leave","expected_revision":6})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"],json!([]));
    join(&h,&id,"bob").await;
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"],json!([]));
    // Project withdrawal never destroys or revokes original task participation.
    assert_eq!(call(&h,"bob","GET","/api/tasks/private-task",Value::Null).await.0,StatusCode::OK);
}

#[tokio::test]
async fn archive_removal_and_account_disable_fail_closed() {
    let h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":"carol","expected_revision":2})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","POST",&format!("/api/projects/{id}/archive"),json!({"expected_revision":3})).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/archive"),json!({"expected_revision":3})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"carol","GET","/api/project-invitations",Value::Null).await.1,json!([]));
    assert_eq!(call(&h,"carol","POST",&format!("/api/projects/{id}/membership"),json!({"action":"accept","expected_revision":4})).await.0,StatusCode::CONFLICT);
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::OK);
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members/bob/remove"),json!({"expected_revision":4})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('alice','Alice','disabled')",[]).unwrap();
    assert_eq!(call(&h,"alice","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tenant_boundaries_cover_invites_and_persisted_projects() {
    let mut h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    h.directory = Arc::new(directory::Directory::new(["alice","bob","carol","eve"].map(String::from)).unwrap().with_tenants(
        [("alice","one"),("bob","two"),("carol","one"),("eve","two")].into_iter().map(|(u,t)|(u.into(),t.into())).collect()).unwrap());
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"bob","GET","/api/projects",Value::Null).await.1,json!([]));
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":"eve","expected_revision":2})).await.0,StatusCode::NOT_FOUND);
    let db = h.db().unwrap();
    db.execute_batch("INSERT INTO tenant_memberships(owner,tenant) VALUES ('alice','one'),('bob','two'),('carol','one'),('eve','two')").unwrap();
    assert!(audit_tenants(&db).is_err());
}

#[tokio::test]
async fn unavailable_creator_does_not_block_withdrawing_consent() {
    let h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    join(&h,&id,"carol").await;
    task(&h);
    let links = format!("/api/projects/{id}/tasks");
    assert_eq!(call(&h,"alice","POST",&links,json!({"task_id":"private-task","label":"Checkpoint","expected_revision":4})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","POST",&format!("{links}/private-task/actions"),json!({"action":"approve","expected_revision":5})).await.0,StatusCode::OK);
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('alice','Alice','disabled')",[]).unwrap();
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    let (_,own) = call(&h,"bob","GET",&format!("/api/projects/{id}/membership"),Value::Null).await;
    assert_eq!(own,json!({"status":"active","revision":6}));
    assert_eq!(call(&h,"bob","POST",&format!("{links}/private-task/actions"),json!({"action":"revoke","expected_revision":6})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"carol","POST",&format!("/api/projects/{id}/membership"),json!({"action":"leave","expected_revision":7})).await.0,StatusCode::OK);
    h.db().unwrap().execute("UPDATE member_profiles SET status='active' WHERE owner='alice'",[]).unwrap();
    let (_,p) = call(&h,"bob","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"],json!([]));
    assert_eq!(call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"carol","GET",&format!("/api/projects/{id}/membership"),Value::Null).await.0,StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn removal_and_reproposal_require_fresh_bilateral_approval() {
    let h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    join(&h,&id,"carol").await;
    task(&h);
    let links = format!("/api/projects/{id}/tasks");
    for active in [false,true] {
        let expected = revision(&h,&id).await;
        assert_eq!(call(&h,"alice","POST",&links,json!({"task_id":"private-task","label":"Checkpoint","expected_revision":expected})).await.0,StatusCode::OK);
        if active {
            assert_eq!(call(&h,"bob","POST",&format!("{links}/private-task/actions"),json!({"action":"approve","expected_revision":expected+1})).await.0,StatusCode::OK);
            h.db().unwrap().execute("UPDATE tasks SET status='delivered',revision=2,result='PRIVATE NEW RESULT' WHERE id='private-task'",[]).unwrap();
            let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
            assert_eq!(p["tasks"][0]["task_status"],"delivered");
            assert!(!p.to_string().contains("PRIVATE"));
        }
        let expected = revision(&h,&id).await;
        assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members/bob/remove"),json!({"expected_revision":expected})).await.0,StatusCode::OK);
        join(&h,&id,"bob").await;
        let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
        assert_eq!(p["tasks"],json!([]));
    }
}

#[tokio::test]
async fn inactive_accounts_are_not_invited_or_projected_and_removed_creator_allows_exit() {
    let h = fixture();
    let id = project(&h).await;
    join(&h,&id,"bob").await;
    join(&h,&id,"carol").await;
    task(&h);
    let links = format!("/api/projects/{id}/tasks");
    assert_eq!(call(&h,"alice","POST",&links,json!({"task_id":"private-task","label":"Checkpoint","expected_revision":4})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","POST",&format!("{links}/private-task/actions"),json!({"action":"approve","expected_revision":5})).await.0,StatusCode::OK);
    for status in ["disabled","invited"] {
        h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('eve','Eve',?1) ON CONFLICT(owner) DO UPDATE SET status=excluded.status",[status]).unwrap();
        assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":"eve","expected_revision":6})).await.0,StatusCode::NOT_FOUND);
    }
    h.db().unwrap().execute("INSERT INTO member_profiles(owner,display_name,status) VALUES ('bob','Bob','disabled')",[]).unwrap();
    let (_,p) = call(&h,"carol","GET",&format!("/api/projects/{id}"),Value::Null).await;
    assert_eq!(p["tasks"],json!([]));
    assert!(!p["members"].to_string().contains("bob"));
    h.directory.replace(directory::Directory::new(["bob","carol","eve"].map(String::from)).unwrap());
    let (_,own) = call(&h,"carol","GET",&format!("/api/projects/{id}/membership"),Value::Null).await;
    assert_eq!(own,json!({"status":"active","revision":6}));
    assert_eq!(call(&h,"carol","POST",&format!("/api/projects/{id}/membership"),json!({"action":"leave","expected_revision":6})).await.0,StatusCode::OK);
}

#[tokio::test]
async fn membership_recovery_never_authorizes_invited_removed_or_unknown_callers() {
    let h = fixture();
    let id = project(&h).await;
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members"),json!({"owner":"bob","expected_revision":0})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}/membership"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"eve","GET",&format!("/api/projects/{id}/membership"),Value::Null).await.0,StatusCode::NOT_FOUND);
    assert_eq!(call(&h,"alice","POST",&format!("/api/projects/{id}/members/bob/remove"),json!({"expected_revision":1})).await.0,StatusCode::OK);
    assert_eq!(call(&h,"bob","GET",&format!("/api/projects/{id}/membership"),Value::Null).await.0,StatusCode::NOT_FOUND);
    for who in ["bob","eve"] {
        assert_eq!(call(&h,who,"POST",&format!("/api/projects/{id}/tasks/private-task/actions"),json!({"action":"revoke","expected_revision":2})).await.0,StatusCode::NOT_FOUND);
    }
}
