//! Owner-confirmed automation grants, persisted with the business database.
use crate::{invalid, Error, Hub, Result};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS automation_permissions (
 owner TEXT PRIMARY KEY REFERENCES sessions(owner),
 revision INTEGER NOT NULL CHECK(revision>=0),
 auto_accept INTEGER NOT NULL CHECK(auto_accept IN (0,1)),
 auto_deliver INTEGER NOT NULL CHECK(auto_deliver IN (0,1)),
 gitlab_execute INTEGER NOT NULL CHECK(gitlab_execute IN (0,1)),
 gitlab_reply INTEGER NOT NULL CHECK(gitlab_reply IN (0,1)));
";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Permissions {
    pub revision: i64,
    pub auto_accept: bool,
    pub auto_deliver: bool,
    pub gitlab_execute: bool,
    pub gitlab_reply: bool,
}

pub(super) fn read(db: &Connection, owner: &str) -> Result<Permissions> {
    Ok(db.query_row("SELECT revision,auto_accept,auto_deliver,gitlab_execute,gitlab_reply FROM automation_permissions WHERE owner=?1", [owner], |r| Ok(Permissions {
        revision:r.get(0)?, auto_accept:r.get(1)?, auto_deliver:r.get(2)?, gitlab_execute:r.get(3)?, gitlab_reply:r.get(4)?
    })).optional()?.unwrap_or_default())
}

pub(super) fn revoke(db: &Connection, owner: &str, p: &Permissions) -> Result<()> {
    db.execute("UPDATE node_assignments SET status='failed' WHERE owner=?1 AND status IN ('queued','running') AND ((authorization='delegation' AND NOT ?2) OR (authorization='gitlab' AND NOT ?3))", params![owner,p.auto_accept,p.gitlab_execute])?;
    Ok(())
}

pub(super) async fn get(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Permissions>> {
    let owner = h.identity(&headers)?;
    Ok(Json(read(&*h.db()?, &owner)?))
}

pub(super) async fn update(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Permissions>,
) -> Result<Json<Permissions>> {
    let owner = h.identity(&headers)?;
    if input.revision < 0 {
        return Err(invalid("invalid permission revision"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let current = read(&tx, &owner)?;
    if current.revision != input.revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "automation permissions changed; reload before saving",
        ));
    }
    let revision = current
        .revision
        .checked_add(1)
        .ok_or_else(|| invalid("permission revision exhausted"))?;
    let next = Permissions { revision, ..input };
    tx.execute("INSERT INTO automation_permissions(owner,revision,auto_accept,auto_deliver,gitlab_execute,gitlab_reply) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(owner) DO UPDATE SET revision=excluded.revision,auto_accept=excluded.auto_accept,auto_deliver=excluded.auto_deliver,gitlab_execute=excluded.gitlab_execute,gitlab_reply=excluded.gitlab_reply",params![owner,next.revision,next.auto_accept,next.auto_deliver,next.gitlab_execute,next.gitlab_reply])?;
    revoke(&tx, &owner, &next)?;
    tx.commit()?;
    Ok(Json(next))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app, Config, Runtime, UserConfig};
    use axum::{body::Body, http::Request};
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    fn hub() -> Hub {
        Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            runtime: Runtime::Mock,
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!(
                        "{:x}",
                        Sha256::digest(format!("{id}-{}", "x".repeat(32)).as_bytes())
                    ),
                })
                .into_iter()
                .collect(),
        })
        .unwrap()
    }
    async fn call(
        h: &Hub,
        user: &str,
        method: &str,
        route: &str,
        data: Value,
    ) -> (StatusCode, Value) {
        let response = app(h.clone())
            .oneshot(
                Request::builder()
                    .method(method)
                    .header("x-agenct-node-protocol", "2")
                    .uri(route)
                    .header(
                        "authorization",
                        format!(
                            "Bearer {}",
                            if matches!(user, "alice" | "bob") {
                                format!("{user}-{}", "x".repeat(32))
                            } else {
                                user.to_owned()
                            }
                        ),
                    )
                    .header("content-type", "application/json")
                    .body(if data.is_null() {
                        Body::empty()
                    } else {
                        Body::from(data.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1000000)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    fn flags(revision: i64, enabled: bool) -> Value {
        json!({"revision":revision,"auto_accept":enabled,"auto_deliver":enabled,"gitlab_execute":enabled,"gitlab_reply":enabled})
    }

    #[tokio::test]
    async fn gitlab_reply_requires_its_own_live_permission() {
        use std::io::{Read, Write};
        let h = hub();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (_, node) = call(
            &h,
            "bob",
            "POST",
            "/api/nodes",
            json!({"id":"pc","name":"PC"}),
        )
        .await;
        call(
            &h,
            "bob",
            "POST",
            "/api/gitlab/config",
            json!({"base_url":base,"token":"test-only-pat","auto_poll_todos":false}),
        )
        .await;
        h.db().unwrap().execute_batch("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('reply-task','bob','bob','Reply task','working',0);
            INSERT INTO node_assignments(task_id,owner,node_id,task_revision,status,updated_at) VALUES ('reply-task','bob','pc',0,'queued',0);").unwrap();
        h.automation.lock().unwrap().gitlab_targets.insert(
            "reply-task".into(),
            crate::sagent::GitLabReplyTarget {
                owner: "bob".into(),
                base_url: base,
                project_path: "team/app".into(),
                noteable_type: "Issue".into(),
                noteable_iid: 1,
            },
        );
        crate::sagent::notify_gitlab_task_completed(&h, "reply-task", "PC", "output");
        assert!(listener.accept().is_err());
        assert!(h
            .automation
            .lock()
            .unwrap()
            .gitlab_targets
            .contains_key("reply-task"));
        let mut permissions = flags(0, false);
        permissions["gitlab_reply"] = json!(true);
        call(&h, "bob", "PUT", "/api/automation/permissions", permissions).await;
        let receiver = std::thread::spawn(move || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                        .unwrap();
                    let mut bytes = [0u8; 8192];
                    let n = stream.read(&mut bytes).unwrap();
                    let request = String::from_utf8_lossy(&bytes[..n]).to_string();
                    stream.write_all(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").unwrap();
                    return request;
                }
                assert!(
                    std::time::Instant::now() < until,
                    "authorized comment never arrived"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        let (_, work) = call(
            &h,
            node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            Value::Null,
        )
        .await;
        let (status,_)=call(&h,node["token"].as_str().unwrap(),"POST","/node/v1/tasks/reply-task/drafts",json!({"lease_token":work["job"]["lease_token"],"request_id":"reply","expected_revision":0,"status":"completed","content":"output"})).await;
        assert_eq!(status, StatusCode::OK);
        for _ in 0..500 {
            if receiver.is_finished() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let request = receiver.join().unwrap();
        assert!(request.starts_with("POST /api/v4/projects/team%2Fapp/issues/1/notes"));
        assert!(request
            .to_ascii_lowercase()
            .contains("private-token: test-only-pat"));
    }

    #[tokio::test]
    async fn defaults_owner_scope_and_revision_checks_protect_permissions() {
        let h = hub();
        let (status, p) = call(
            &h,
            "alice",
            "GET",
            "/api/automation/permissions",
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(p, flags(0, false));
        assert_eq!(
            call(
                &h,
                "alice",
                "PUT",
                "/api/automation/permissions",
                flags(0, true)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert!(!read(&h.db().unwrap(), "bob").unwrap().auto_accept);
        assert_eq!(
            call(
                &h,
                "alice",
                "PUT",
                "/api/automation/permissions",
                flags(0, false)
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let mut forged = flags(1, true);
        forged["owner"] = json!("bob");
        assert!(
            !call(&h, "alice", "PUT", "/api/automation/permissions", forged)
                .await
                .0
                .is_success()
        );
        assert!(!call(
            &h,
            "invalid",
            "GET",
            "/api/automation/permissions",
            Value::Null
        )
        .await
        .0
        .is_success());
    }

    #[tokio::test]
    async fn registering_node_and_supplying_clarification_do_not_authorize_acceptance() {
        let h = hub();
        assert_eq!(
            call(
                &h,
                "bob",
                "POST",
                "/api/nodes",
                json!({"id":"pc","name":"PC"})
            )
            .await
            .0,
            StatusCode::OK
        );
        {
            let db = h.db().unwrap();
            db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','Fix issue','awaiting_owner',0)",[]).unwrap();
            crate::sagent::on_task_delegated(&db, &h, "task", "alice", "bob", "Fix issue", None)
                .unwrap();
            assert_eq!(
                db.query_row("SELECT status FROM tasks WHERE id='task'", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "awaiting_owner"
            );
        }
        call(
            &h,
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"补充：请开始执行"}),
        )
        .await;
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT count(*) FROM node_assignments", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        call(
            &h,
            "bob",
            "PUT",
            "/api/automation/permissions",
            flags(0, true),
        )
        .await;
        call(
            &h,
            "bob",
            "POST",
            "/api/automation/rules",
            json!({"keyword":"*","require_human_approval":true}),
        )
        .await;
        call(
            &h,
            "alice",
            "POST",
            "/api/messages",
            json!({"message":"补充：信息已经齐全"}),
        )
        .await;
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT status FROM tasks WHERE id='task'", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "awaiting_owner"
        );
    }

    #[tokio::test]
    async fn revocation_cancels_automatic_jobs_and_reenable_does_not_resurrect_them() {
        let h = hub();
        let (_, node) = call(
            &h,
            "bob",
            "POST",
            "/api/nodes",
            json!({"id":"pc","name":"PC"}),
        )
        .await;
        call(
            &h,
            "bob",
            "PUT",
            "/api/automation/permissions",
            flags(0, true),
        )
        .await;
        {
            let db = h.db().unwrap();
            for (id, source) in [
                ("delegated", "delegation"),
                ("external", "gitlab"),
                ("manual", "manual"),
            ] {
                db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES (?1,'bob','bob',?1,'working',0)",[id]).unwrap();
                db.execute("INSERT INTO node_assignments(task_id,owner,node_id,task_revision,status,updated_at,authorization) VALUES (?1,'bob','pc',0,'queued',0,?2)",params![id,source]).unwrap();
            }
        }
        assert_eq!(
            call(
                &h,
                "bob",
                "PUT",
                "/api/automation/permissions",
                flags(1, false)
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                &h,
                "bob",
                "PUT",
                "/api/automation/permissions",
                flags(2, true)
            )
            .await
            .0,
            StatusCode::OK
        );
        for id in ["delegated", "external"] {
            assert_eq!(
                h.db()
                    .unwrap()
                    .query_row(
                        "SELECT status FROM node_assignments WHERE task_id=?1",
                        [id],
                        |r| r.get::<_, String>(0)
                    )
                    .unwrap(),
                "failed"
            );
            assert_eq!(call(&h,node["token"].as_str().unwrap(),"POST",&format!("/node/v1/tasks/{id}/drafts"),json!({"request_id":id,"expected_revision":0,"status":"completed","content":"private output"})).await.0,StatusCode::CONFLICT);
        }
        let (_, work) = call(
            &h,
            node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            Value::Null,
        )
        .await;
        assert_eq!(work["job"]["task_id"], "manual");
    }

    #[tokio::test]
    async fn auto_accept_does_not_grant_delivery_or_gitlab_execution() {
        let h = hub();
        let (_, node) = call(
            &h,
            "bob",
            "POST",
            "/api/nodes",
            json!({"id":"pc","name":"PC"}),
        )
        .await;
        let mut p = flags(0, false);
        p["auto_accept"] = json!(true);
        call(&h, "bob", "PUT", "/api/automation/permissions", p).await;
        {
            let db = h.db().unwrap();
            db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','alice','bob','Write report','awaiting_owner',0)",[]).unwrap();
            crate::sagent::on_task_delegated(&db, &h, "task", "alice", "bob", "Write report", None)
                .unwrap();
        }
        let (_, work) = call(
            &h,
            node["token"].as_str().unwrap(),
            "GET",
            "/node/v1/work",
            Value::Null,
        )
        .await;
        let (status,_)=call(&h,node["token"].as_str().unwrap(),"POST","/node/v1/tasks/task/drafts",json!({"lease_token":work["job"]["lease_token"],"request_id":"draft","expected_revision":1,"status":"completed","content":"private report"})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            h.db()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM deliveries WHERE status='submitted'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        let (status, hook) = call(
            &h,
            "bob",
            "POST",
            "/webhooks/gitlab",
            json!({"body":"@bob execute external task"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(hook["triggered"][0].get("task_id").is_none());
    }
}
