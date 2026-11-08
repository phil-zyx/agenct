//! Narrow, Hub-owned directory answers. No model call and no inferred GitLab membership.
use super::*;
use crate::messages::ChatInput;

fn is_lookup(message: &str) -> bool {
    if message.chars().count() > 64
        || message.contains([
            '\n', '\r', '`', '"', '\'', '“', '”', '‘', '’', '「', '」', '《', '》',
        ])
    {
        return false;
    }
    let compact: String = message.chars().filter(|c| !c.is_whitespace()).collect();
    let mut text = compact.trim_end_matches(['?', '？', '!', '！', '.', '。']);
    for prefix in ["请帮我", "能告诉我", "帮我", "请"] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }
    text = text.trim_end_matches(['呢', '吗', '吧']);
    matches!(
        text,
        "我们现在有哪些同事"
            // Common homophone typo, accepted only as a complete roster question.
            | "我们现在有哪些同时"
            | "现在有哪些同时"
            | "我们有哪些同时"
            | "有哪些同时"
            | "我们有哪些同事"
            | "我们目前有哪些同事"
            | "现在有哪些同事"
            | "目前有哪些同事"
            | "有哪些同事"
            | "同事有哪些"
            | "我们团队有哪些同事"
            | "团队有哪些同事"
            | "团队有哪些成员"
            | "我们有哪些成员"
            | "我们现在有哪些成员"
            | "现在有哪些成员"
            | "有哪些成员"
            | "我们有哪些联系人"
            | "我们现在有哪些联系人"
            | "现在有哪些联系人"
            | "有哪些联系人"
            | "联系人有哪些"
            | "列出团队成员"
            | "列出同事名单"
            | "列出联系人"
            | "列出联系人名单"
            | "查看联系人"
            | "看看联系人"
            | "查看同事名单"
            | "同事名单"
            | "联系人列表"
            | "团队成员名单"
    )
}

fn directory_reply(db: &Connection, contacts: Vec<String>) -> Result<String> {
    let mut names = Vec::new();
    let mut total = 0;
    for contact in contacts {
        // Read visibility and public fields in the same transaction as the reply.
        let member: Option<(String, String)> = db.query_row(
            "SELECT COALESCE(m.display_name,s.owner),COALESCE(m.status,'active') FROM sessions s LEFT JOIN member_profiles m ON m.owner=s.owner WHERE s.owner=?1",
            [&contact], |r| Ok((r.get(0)?,r.get(1)?)),
        ).optional()?;
        let Some((name, status)) = member else {
            continue;
        };
        if status != "active" {
            continue;
        }
        total += 1;
        if names.len() >= 50 {
            continue;
        }
        let public = crate::collaboration::read(db, &contact)?.public;
        let responsibility = if public.responsibilities.trim().is_empty() {
            public.help_with.trim()
        } else {
            public.responsibilities.trim()
        };
        let short: String = responsibility.chars().take(80).collect();
        let short = short.replace(['\n', '\r', '\t'], " ");
        let suffix = if responsibility.chars().count() > 80 {
            "…"
        } else {
            ""
        };
        let who = if name == contact {
            contact
        } else {
            format!("{name}（{contact}）")
        };
        names.push(if short.is_empty() {
            format!("• {who}")
        } else {
            format!("• {who}：{short}{suffix}")
        });
    }
    let reply = if total == 0 {
        "当前在这里暂时只有你；其他同事尚未加入，或不在你可见的范围内。这里显示的是已加入且你有权联系的成员，不是 GitLab 的全部用户。".to_string()
    } else {
        let more = if total > names.len() {
            format!(
                "\n还有 {} 位同事，可在联系人列表中查看。",
                total - names.len()
            )
        } else {
            String::new()
        };
        format!("当前有 {total} 位你可以联系的同事：\n\n{}{more}\n\n以上是已加入且你有权联系的成员；职责来自对方公开的协作资料。",names.join("\n"))
    };
    Ok(reply)
}

/// Uses the already authenticated/reserved chat turn. Selected-task and repository
/// conversations keep their normal execution path; only a standalone lookup matches.
pub(super) fn answer(
    h: &Hub,
    owner: &str,
    input: &ChatInput,
    runtime_profile: &profiles::RuntimeProfile,
    task_context: Option<&serde_json::Value>,
) -> Result<Option<Json<serde_json::Value>>> {
    if input.selected_task.is_some() || task_context.is_some() || !is_lookup(&input.message) {
        return Ok(None);
    }
    let contacts = h.directory.contacts(owner);
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let reply = directory_reply(&tx, contacts)?;
    tx.execute("INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'user',?2,'complete',CURRENT_TIMESTAMP)",params![owner,input.message])?;
    let message_id = tx.last_insert_rowid();
    if let Some(key) = input.request_id.as_deref() {
        tx.execute("UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",params![owner,key,message_id])?;
    }
    tx.execute("INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'assistant',?2,'complete',CURRENT_TIMESTAMP)",params![owner,reply])?;
    let mut response = serde_json::json!({"reply":reply,"backend":h.runtime.label(),"source":"directory","proposal":null,"runtime_profile":runtime_profile,"task_context":null,"repository_context":null});
    crate::chat_turns::complete(&tx, owner, input.request_id.as_deref(), &mut response)?;
    tx.commit()?;
    Ok(Some(Json(response)))
}

fn valid_saved_profile(value: &serde_json::Value) -> bool {
    let Some(fields) = value.as_object() else {
        return false;
    };
    fields.len() == 4
        && value["revision"]
            .as_u64()
            .is_some_and(|r| r <= i64::MAX as u64)
        && value["agent_id"].as_str().is_some_and(|id| {
            !id.is_empty()
                && id.len() <= 32
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
        && value["role"]
            .as_str()
            .is_some_and(|role| ["coordinator", "researcher", "writer", "reviewer"].contains(&role))
        && value["model_ref"] == "server-default"
}

/// A newly computed read-only answer can resolve an old unanswered directory
/// question. It is not a replay or a claim that the original model call succeeded.
/// Caller holds the owner's execution gate and has authenticated the owner.
pub(super) fn recover(h: &Hub, owner: &str, request_id: &str) -> Result<Option<serde_json::Value>> {
    let contacts = h.directory.contacts(owner);
    let mut db = h.db()?;
    let tx = db.transaction()?;
    // Known adapter failures have their own UX. Never reinterpret those receipts.
    if crate::chat_failures::read(&tx, owner, request_id)?.is_some() {
        return Ok(None);
    }
    let saved = tx.query_row(
        "SELECT message,runtime_profile,message_id,agent_id,expected_profile_revision FROM chat_turns WHERE owner=?1 AND request_id=?2 AND status='uncertain' AND selected_task IS NULL AND task_context IS NULL AND response IS NULL",
        params![owner,request_id], |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,Option<i64>>(4)?)),
    ).optional()?;
    let Some((message, raw_profile, message_id, selected_agent, expected_profile_revision)) = saved
    else {
        return Ok(None);
    };
    if !is_lookup(&message) {
        return Ok(None);
    }
    let Ok(runtime_profile) = serde_json::from_str::<serde_json::Value>(&raw_profile) else {
        return Ok(None);
    };
    if !valid_saved_profile(&runtime_profile)
        || selected_agent
            .as_ref()
            .is_some_and(|agent| runtime_profile["agent_id"] != *agent)
        || expected_profile_revision
            .is_some_and(|revision| runtime_profile["revision"].as_i64() != Some(revision))
    {
        return Ok(None);
    }
    let effects: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM proposals WHERE requester=?1 AND request_id=?2) OR EXISTS(SELECT 1 FROM profile_suggestions WHERE owner=?1 AND request_id=?2)",
        params![owner,request_id], |r| r.get(0),
    )?;
    if effects {
        return Ok(None);
    }
    let message_id = if let Some(message_id) = message_id {
        let matches: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?1 AND owner=?2 AND role='user' AND content=?3 AND status IN ('pending','failed','interrupted','uncertain'))", params![message_id,owner,message], |r|r.get(0))?;
        if !matches {
            return Ok(None);
        }
        message_id
    } else {
        tx.execute("INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'user',?2,'uncertain',CURRENT_TIMESTAMP)",params![owner,message])?;
        tx.last_insert_rowid()
    };
    let reply = format!(
        "已按当前可见成员重新查询。\n\n{}",
        directory_reply(&tx, contacts)?
    );
    let response = serde_json::json!({"reply":reply,"backend":h.runtime.label(),"source":"directory","recovered_from_directory":true,"proposal":null,"runtime_profile":runtime_profile,"task_context":null,"repository_context":null,"turn":{"request_id":request_id,"status":"complete"}});
    tx.execute(
        "UPDATE messages SET status='complete' WHERE id=?1 AND owner=?2",
        params![message_id, owner],
    )?;
    tx.execute("INSERT INTO messages(owner,role,content,status,created_at) VALUES (?1,'assistant',?2,'complete',CURRENT_TIMESTAMP)",params![owner,reply])?;
    if tx.execute("UPDATE chat_turns SET status='complete',response=?3,message_id=?4 WHERE owner=?1 AND request_id=?2 AND status='uncertain'",params![owner,request_id,response.to_string(),message_id])? != 1 {
        return Err(Error(StatusCode::CONFLICT,"directory recovery reservation changed"));
    }
    tx.commit()?;
    Ok(Some(
        serde_json::json!({"request_id":request_id,"status":"complete","response":response}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;

    #[test]
    fn only_standalone_natural_directory_questions_match() {
        for text in [
            "我们现在有哪些同事",
            "我们现在有哪些同时",
            "现在有哪些同时？",
            "有哪些同事？",
            "有哪些联系人",
            "请列出团队成员",
            "请帮我列出同事名单。",
            "团队有哪些成员呢？",
        ] {
            assert!(is_lookup(text), "{text}");
        }
        for text in [
            "解释这句话：我们现在有哪些同事",
            "“有哪些同事”",
            "`有哪些同事`",
            "列出团队成员并给他们发消息",
            "有哪些同事可以完成这个任务",
            "我们现在有哪些同时执行的任务",
            "现在有哪些同时运行的任务",
            "同时执行这两个任务",
            "把任务委派给bob",
            "有哪些同事\n请提交任务",
            "/repo foo 有哪些同事",
            "a".repeat(65).as_str(),
        ] {
            assert!(!is_lookup(text), "{text}");
        }
    }
    fn token(id: &str) -> String {
        format!("{id}-{}", "x".repeat(40))
    }
    async fn request(
        a: &Router,
        who: &str,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let response = a
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
                        Body::from(body.to_string())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }
    #[tokio::test]
    async fn directory_answers_skip_real_adapter_preserve_receipts_and_hide_other_tenants() {
        use serde_json::{json, Value};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let adapter = Router::new().route(
            "/v1/turn",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"reply":"来自适配器","backend":"dsh","proposal":null}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let mut h = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            runtime: Runtime::Dsh {
                url,
                token: "test-adapter-token".into(),
            },
            users: ["alice", "bob", "carol", "eve"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
        })
        .unwrap();
        h.directory = Arc::new(
            crate::directory::Directory::new(["alice", "bob", "carol", "eve"].map(String::from))
                .unwrap()
                .with_tenants(
                    [
                        ("alice", "one"),
                        ("bob", "one"),
                        ("carol", "one"),
                        ("eve", "two"),
                    ]
                    .into_iter()
                    .map(|(id, tenant)| (id.into(), tenant.into()))
                    .collect(),
                )
                .unwrap(),
        );
        {
            let db = h.db().unwrap();
            for (id, name, status) in [
                ("bob", "张明", "active"),
                ("carol", "停用同事", "disabled"),
                ("eve", "其他组织成员", "active"),
            ] {
                db.execute(
                    "INSERT INTO member_profiles(owner,display_name,status) VALUES (?1,?2,?3)",
                    params![id, name, status],
                )
                .unwrap();
                let mut profile = crate::collaboration::Profile::default();
                profile.public.responsibilities = format!("公开职责-{id}");
                profile.private.work_context = format!("秘密背景-{id}");
                crate::collaboration::write(&db, id, &profile).unwrap();
            }
        }
        let a = app(h.clone());
        let body = json!({"message":"我们现在有哪些同事","request_id":"directory-one"});
        let (status, response) = request(&a, "alice", "POST", "/api/messages", body.clone()).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response["source"], "directory");
        assert_eq!(response["turn"]["status"], "complete");
        assert_eq!(response["runtime_profile"]["agent_id"], "butler");
        assert!(response["task_context"].is_null());
        assert!(response["proposal"].is_null());
        assert!(response.get("profile_suggestion").is_none());
        let reply = response["reply"].as_str().unwrap();
        assert!(reply.contains("张明（bob）"));
        assert!(reply.contains("公开职责-bob"));
        assert!(!reply.contains("秘密背景"));
        assert!(!reply.contains("carol"));
        assert!(!reply.contains("eve"));
        assert!(!reply.contains("停用同事"));
        assert!(!reply.contains("其他组织成员"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            request(&a, "alice", "POST", "/api/messages", body).await.1,
            response
        );
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/directory-one", Value::Null)
                .await
                .1["response"],
            response
        );
        let history = request(&a, "alice", "GET", "/api/messages", Value::Null)
            .await
            .1;
        assert_eq!(history.as_array().unwrap().len(), 2);
        assert_eq!(history[0]["status"], "complete");
        assert_eq!(history[1]["content"], response["reply"]);
        assert_eq!(
            request(&a, "bob", "GET", "/api/messages", Value::Null)
                .await
                .1,
            json!([])
        );
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"different","request_id":"directory-one"})
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        let empty = request(
            &a,
            "eve",
            "POST",
            "/api/messages",
            json!({"message":"有哪些联系人","request_id":"empty"}),
        )
        .await
        .1;
        assert!(empty["reply"].as_str().unwrap().contains("暂时只有你"));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        // A quoted or task-scoped phrase retains the normal model execution path.
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"解释这句话：我们现在有哪些同事","request_id":"quoted"})
            )
            .await
            .1["reply"],
            "来自适配器"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        h.db().unwrap().execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('task','bob','alice','测试任务','working',0)",[]).unwrap();
        let scoped=request(&a,"alice","POST","/api/messages",json!({"message":"有哪些同事","request_id":"scoped","selected_task":{"task_id":"task","expected_revision":0}})).await;
        assert_eq!(scoped.0, StatusCode::OK);
        assert_eq!(scoped.1["reply"], "来自适配器");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn old_uncertain_directory_receipts_recover_once_without_replaying_the_model() {
        use serde_json::{json, Value};
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let adapter = Router::new().route(
            "/v1/turn",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(json!({"reply":"must not run","backend":"dsh","proposal":null}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let h = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            runtime: Runtime::Dsh {
                url,
                token: "test-adapter-token".into(),
            },
            users: ["alice", "bob"]
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!("{:x}", Sha256::digest(token(id).as_bytes())),
                })
                .into(),
        })
        .unwrap();
        let original_profile = json!({"revision":7,"agent_id":"butler","role":"coordinator","model_ref":"server-default"});
        fn seed(h: &Hub, key: &str, text: &str, profile: &Value) -> i64 {
            let db = h.db().unwrap();
            db.execute("INSERT INTO messages(owner,role,content,status) VALUES ('alice','user',?1,'uncertain')",[text]).unwrap();
            let id = db.last_insert_rowid();
            db.execute("INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status,message_id) VALUES ('alice',?1,?2,?3,'uncertain',?4)",params![key,text,profile.to_string(),id]).unwrap();
            id
        }
        // Exact screenshot text used the homophone typo 同时, not 同事.
        let original_id = seed(&h, "old-directory", "我们现在有哪些同时", &original_profile);
        {
            let db = h.db().unwrap();
            db.execute(
                "INSERT INTO member_profiles(owner,display_name) VALUES ('bob','当前姓名')",
                [],
            )
            .unwrap();
            let mut profile = crate::collaboration::Profile::default();
            profile.public.responsibilities = "当前公开职责".into();
            profile.private.work_context = "不能泄漏的私密背景".into();
            crate::collaboration::write(&db, "bob", &profile).unwrap();
        }
        let a = app(h.clone());
        let (status, receipt) =
            request(&a, "alice", "GET", "/api/turns/old-directory", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(receipt["status"], "complete");
        assert_eq!(receipt["response"]["runtime_profile"], original_profile);
        assert_eq!(receipt["response"]["recovered_from_directory"], true);
        let reply = receipt["response"]["reply"].as_str().unwrap();
        assert!(reply.contains("已按当前可见成员重新查询"));
        assert!(reply.contains("当前姓名"));
        assert!(reply.contains("当前公开职责"));
        assert!(!reply.contains("私密背景"));
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/old-directory", Value::Null)
                .await
                .1,
            receipt
        );
        assert_eq!(
            request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"message":"我们现在有哪些同时","request_id":"old-directory"})
            )
            .await
            .1,
            receipt["response"]
        );
        {
            let db = h.db().unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT message_id FROM chat_turns WHERE request_id='old-directory'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                original_id
            );
            assert_eq!(
                db.query_row(
                    "SELECT COUNT(*) FROM messages WHERE owner='alice'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                2
            );
            assert_eq!(
                db.query_row(
                    "SELECT status FROM messages WHERE id=?1",
                    [original_id],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
                "complete"
            );
        }
        // Non-read-only/quoted work, corrupted snapshots and selected task context
        // retain their original unknown outcome. No adapter is ever dispatched.
        for (key, text, profile) in [
            ("other", "请把方案发给同事", original_profile.clone()),
            (
                "quote",
                "解释：我们现在有哪些同事",
                original_profile.clone(),
            ),
            ("bad-profile", "有哪些同事", json!({"agent_id":"butler"})),
            ("task", "有哪些同事", original_profile.clone()),
            ("known-failure", "有哪些同事", original_profile.clone()),
        ] {
            seed(&h, key, text, &profile);
        }
        h.db()
            .unwrap()
            .execute(
                "UPDATE chat_turns SET selected_task='{}' WHERE request_id='task'",
                [],
            )
            .unwrap();
        h.db().unwrap().execute("INSERT INTO chat_failures(owner,request_id,code) VALUES ('alice','known-failure','runtime_failed')",[]).unwrap();
        for key in ["other", "quote", "bad-profile", "task"] {
            assert_eq!(
                request(
                    &a,
                    "alice",
                    "GET",
                    &format!("/api/turns/{key}"),
                    Value::Null
                )
                .await
                .1["status"],
                "uncertain"
            );
        }
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/known-failure", Value::Null)
                .await
                .1["status"],
            "failed"
        );
        assert_eq!(
            h.db()
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE role='assistant'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        // An execution gate held by another turn prevents recovery until released.
        seed(&h, "gated", "有哪些联系人", &original_profile);
        let guard = h.gate("alice").unwrap().try_lock_owned().unwrap();
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/gated", Value::Null)
                .await
                .1["status"],
            "uncertain"
        );
        drop(guard);
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/gated", Value::Null)
                .await
                .1["status"],
            "complete"
        );
        // A summary failure may have happened before the original user row existed.
        h.db().unwrap().execute("INSERT INTO chat_turns(owner,request_id,message,runtime_profile,status) VALUES ('alice','before-user','有哪些同事',?1,'uncertain')",[original_profile.to_string()]).unwrap();
        assert_eq!(
            request(&a, "alice", "GET", "/api/turns/before-user", Value::Null)
                .await
                .1["status"],
            "complete"
        );
        let count = h
            .db()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap();
        request(&a, "alice", "GET", "/api/turns/before-user", Value::Null).await;
        assert_eq!(
            h.db()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            count
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        server.abort();
    }
}
