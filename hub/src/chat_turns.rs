//! Durable, owner-private at-most-once dispatch receipts, not provider exactly-once execution.
use super::messages::{execute_chat, ChatInput};
use super::*;

fn mark_uncertain(db: &mut Connection, owner: &str, key: &str) -> Result<()> {
    let tx = db.transaction()?;
    tx.execute("UPDATE messages SET status='uncertain' WHERE owner=?1 AND status IN ('pending','failed','interrupted') AND id IN (SELECT message_id FROM chat_turns WHERE owner=?1 AND request_id=?2 AND status='pending')", params![owner,key])?;
    tx.execute("UPDATE chat_turns SET status='uncertain' WHERE owner=?1 AND request_id=?2 AND status='pending'", params![owner,key])?;
    tx.commit()?;
    Ok(())
}
fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
fn receipt(db: &Connection, owner: &str, key: &str) -> Result<Option<serde_json::Value>> {
    let row = db
        .query_row(
            "SELECT status,response FROM chat_turns WHERE owner=?1 AND request_id=?2",
            params![owner, key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    row.map(|(status, response)| {
        let response = response
            .map(|raw| serde_json::from_str::<serde_json::Value>(&raw))
            .transpose()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "invalid stored turn"))?;
        if status == "uncertain" {
            if let Some(failure) = crate::chat_failures::read(db, owner, key)? {
                return Ok(serde_json::json!({"request_id":key,"status":"failed","response":null,"failure":failure}));
            }
        }
        Ok(serde_json::json!({"request_id":key,"status":status,"response":response}))
    })
    .transpose()
}
fn replay(
    h: &Hub,
    db: &Connection,
    owner: &str,
    input: &ChatInput,
) -> Result<Option<serde_json::Value>> {
    let Some(key) = input.request_id.as_deref() else {
        return Ok(None);
    };
    let binding = db
        .query_row(
            "SELECT message,agent_id,selected_task,expected_profile_revision,selected_project FROM chat_turns WHERE owner=?1 AND request_id=?2",
            params![owner, key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<i64>>(3)?,r.get::<_,Option<String>>(4)?)),
        )
        .optional()?;
    let Some((message, agent_id, selected_task, expected_profile_revision, selected_project)) =
        binding
    else {
        return Ok(None);
    };
    let selection = input
        .selected_task
        .as_ref()
        .map(|s| serde_json::to_string(s).expect("selection JSON"));
    if message != input.message
        || agent_id != input.agent_id
        || selected_task != selection
        || expected_profile_revision != input.expected_profile_revision
        || selected_project
            != input
                .selected_project
                .as_ref()
                .map(|s| serde_json::to_string(s).expect("selection JSON"))
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "request_id already used for different content, agent, profile revision, selected task or project",
        ));
    }
    if let Some(selection) = &input.selected_project {
        projects::authorize(h, db, &selection.project_id, owner)?;
    }
    let found = receipt(db, owner, key)?.ok_or_else(denied)?;
    if found["status"] == "complete" {
        return Ok(Some(found["response"].clone()));
    }
    if found["status"] == "failed" {
        return Ok(Some(crate::chat_failures::response(
            key,
            found["failure"].clone(),
        )));
    }
    Ok(Some(
        serde_json::json!({"reply":null,"turn":{"request_id":key,"status":found["status"]}}),
    ))
}
/// Called inside the same transaction as assistant messages and proposals.
pub(super) fn complete(
    tx: &rusqlite::Transaction<'_>,
    owner: &str,
    key: Option<&str>,
    response: &mut serde_json::Value,
) -> Result<()> {
    if let Some(key) = key {
        response["turn"] = serde_json::json!({"request_id":key,"status":"complete"});
        if tx.execute("UPDATE chat_turns SET status='complete',response=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'",
            params![owner,key,response.to_string()])? != 1 {
            return Err(Error(StatusCode::CONFLICT,"turn reservation unavailable"));
        }
    }
    Ok(())
}
pub(super) async fn chat(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<ChatInput>,
) -> Result<Json<serde_json::Value>> {
    let id = h.identity(&headers)?;
    if input.selected_project.as_ref().is_some_and(|s| !s.valid())
        || (input.selected_project.is_some() && input.selected_task.is_some())
    {
        return Err(invalid("invalid or conflicting selected project"));
    }
    if input.message.trim().is_empty() || input.message.len() > 8000 {
        return Err(invalid("message must contain 1..8000 bytes"));
    }
    if input
        .request_id
        .as_deref()
        .is_some_and(|key| !valid_key(key))
    {
        return Err(invalid("invalid request_id"));
    }
    if input
        .selected_task
        .as_ref()
        .is_some_and(|selected| !selected.valid())
    {
        return Err(invalid("invalid selected task"));
    }
    if input.agent_id.as_ref().is_some_and(|id| {
        id.is_empty()
            || id.len() > 32
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    }) || input
        .expected_profile_revision
        .is_some_and(|revision| revision < 0 || input.agent_id.is_none())
    {
        return Err(invalid("invalid explicit agent selection"));
    }
    // Completed/uncertain replays are independent of current profile and active turns.
    if let Some(response) = replay(&h, &*h.db()?, &id, &input)? {
        return Ok(Json(response));
    }
    let guard = h
        .gate(&id)?
        .try_lock_owned()
        .map_err(|_| Error(StatusCode::CONFLICT, "a turn is already running"))?;
    if let Some(response) = replay(&h, &*h.db()?, &id, &input)? {
        return Ok(Json(response));
    }
    let (runtime_profile, task_context, project_context) = {
        // Capture and reserve under the shared profile/task mutation mutex.
        // Later edits cannot alter the frozen routing or discussion snapshot.
        let db = h.db()?;
        let runtime_profile = profiles::route(
            &db,
            &id,
            input.agent_id.as_deref(),
            input.expected_profile_revision,
        )?;
        let project_context =
            crate::project_context::capture(&h, &db, &id, input.selected_project.as_ref())?;
        let task_context = task_context::capture(&db, &id, input.selected_task.as_ref())?;
        if let Some(key) = input.request_id.as_deref() {
            let collision: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM proposals WHERE requester=?1 AND request_id=?2)",
                params![id, key],
                |r| r.get(0),
            )?;
            if collision {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "request_id already used by proposal endpoint",
                ));
            }
            let selection = input
                .selected_task
                .as_ref()
                .map(|s| serde_json::to_string(s).expect("selection JSON"));
            db.execute("INSERT INTO chat_turns(owner,request_id,message,agent_id,runtime_profile,status,selected_task,task_context,expected_profile_revision,selected_project) VALUES (?1,?2,?3,?4,?5,'pending',?6,?7,?8,?9)",
                params![id,key,input.message,input.agent_id,serde_json::to_string(&runtime_profile).map_err(|_| invalid("invalid runtime profile"))?,selection,task_context.as_ref().map(|c|c.to_string()),input.expected_profile_revision,input.selected_project.as_ref().map(|s| serde_json::to_string(s).expect("selection JSON"))])?;
        }
        (runtime_profile, task_context, project_context)
    };
    // Dispatch in a detached task that owns the per-user gate. If the client
    // disconnects (tab refresh, network drop), the single dispatch still runs to
    // completion and its receipt is recorded, so a later status check returns the
    // real outcome instead of a receipt stranded in `pending`. Still at most once.
    let key = input.request_id.clone();
    let (task_hub, task_id) = (h.clone(), id.clone());
    let dispatch = tokio::spawn(async move {
        let _guard = guard;
        let result = if let Some(context) = project_context {
            crate::project_context::execute(&task_hub, &task_id, &input, runtime_profile, context)
                .await
        } else {
            execute_chat(&task_hub, &task_id, &input, runtime_profile, task_context).await
        };
        if let Err(error) = &result {
            if let Some(key) = input.request_id.as_deref() {
                if let Some(code) = crate::chat_failures::code(error) {
                    // Only a strict adapter error receipt proves this dispatch
                    // ended without a usable reply. It does not prove no model
                    // work happened, and this reservation is never dispatched again.
                    let mut db = task_hub.db()?;
                    crate::chat_failures::record(&mut db, &task_id, key, code)?;
                    if let Some(failure) = crate::chat_failures::read(&db, &task_id, key)? {
                        return Ok(Json(crate::chat_failures::response(key, failure)));
                    }
                    return result;
                }
                // Transport timeout, invalid response or process interruption
                // cannot establish the outcome. Keep the conservative receipt.
                mark_uncertain(&mut *task_hub.db()?, &task_id, key)?;
            }
        }
        result
    });
    match dispatch.await {
        Ok(result) => result,
        Err(_) => {
            // Panicked dispatch: the outcome is unknown, never "not sent".
            if let Some(key) = key.as_deref() {
                mark_uncertain(&mut *h.db()?, &id, key)?;
            }
            Err(Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "chat dispatch ended unexpectedly; outcome unknown",
            ))
        }
    }
}
pub(super) async fn status(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let id = h.identity(&headers)?;
    if !valid_key(&key) {
        return Err(denied());
    }
    // A dropped handler releases the gate. Its reserved dispatch remains uncertain.
    if let Ok(_guard) = h.gate(&id)?.try_lock_owned() {
        mark_uncertain(&mut *h.db()?, &id, &key)?;
        let saved = receipt(&*h.db()?, &id, &key)?;
        if saved
            .as_ref()
            .is_some_and(|saved| saved["status"] == "uncertain")
        {
            if let Some(recovered) = crate::colleague_lookup::recover(&h, &id, &key)? {
                return Ok(Json(recovered));
            }
        }
    }
    // Authorization and retrieval must share the same lock: membership removal
    // cannot interleave between checking access and reading the project snapshot.
    let db = h.db()?;
    let selection: Option<String> = db
        .query_row(
            "SELECT selected_project FROM chat_turns WHERE owner=?1 AND request_id=?2",
            params![id, key],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    if let Some(raw) = selection {
        let selected: crate::project_context::Selection =
            serde_json::from_str(&raw).map_err(|_| invalid("invalid stored project selection"))?;
        projects::authorize(&h, &db, &selected.project_id, &id)?;
    }
    Ok(Json(receipt(&db, &id, &key)?.ok_or_else(denied)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::history;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn failure_config(path: &str, runtime: Runtime) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: path.into(),
            users: vec![UserConfig {
                id: "alice".into(),
                token_sha256: format!("{:x}", Sha256::digest("a".repeat(40).as_bytes())),
            }],
            runtime,
        }
    }

    fn failed_input() -> ChatInput {
        ChatInput {
            message: "hello".into(),
            agent_id: None,
            expected_profile_revision: None,
            request_id: Some("failure-fixture".into()),
            selected_task: None,
            selected_project: None,
        }
    }

    async fn failure_adapter(
        response_status: u16,
        content_type: &'static str,
        body: String,
        path: &str,
    ) -> (
        Hub,
        HeaderMap,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let adapter = Router::new().route(
            "/v1/turn",
            post(move || {
                let (counter, body) = (counter.clone(), body.clone());
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::response::Response::builder()
                        .status(response_status)
                        .header("content-type", content_type)
                        .body(axum::body::Body::from(body))
                        .unwrap()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let hub = Hub::open(failure_config(
            path,
            Runtime::Dsh {
                url,
                token: "fixture-adapter-token".into(),
            },
        ))
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", "a".repeat(40)).parse().unwrap(),
        );
        (hub, headers, calls, server)
    }

    #[tokio::test]
    async fn explicit_adapter_errors_are_durable_failed_receipts_without_redispatch() {
        for (response_status, code) in [
            (502, "runtime_failed"),
            (504, "runtime_timeout"),
            (400, "invalid_turn"),
            (503, "busy"),
            (502, "provider_not_configured"),
            (502, "sdk_version_mismatch"),
            (502, "sdk_unavailable"),
            (502, "model_protocol_error"),
            (502, "runtime_incomplete"),
        ] {
            let path =
                std::env::temp_dir().join(format!("agenct-chat-failure-{}.db", Uuid::new_v4()));
            let (h, headers, calls, server) = failure_adapter(
                response_status,
                "application/json; charset=utf-8",
                serde_json::json!({"error":code}).to_string(),
                path.to_str().unwrap(),
            )
            .await;
            let runtime = h.runtime.clone();
            let result = chat(State(h.clone()), headers.clone(), Json(failed_input()))
                .await
                .unwrap()
                .0;
            assert_eq!(result["turn"]["status"], "failed");
            assert_eq!(result["failure"]["code"], code);
            assert_eq!(
                result["failure"]["message"],
                "这次没有取得回复，可重新提问。"
            );
            assert_eq!(history(&h, "alice").unwrap()[0].status, "failed");
            let observed = status(
                State(h.clone()),
                headers.clone(),
                Path("failure-fixture".into()),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(observed["status"], "failed");
            assert_eq!(observed["failure"], result["failure"]);
            assert!(observed["response"].is_null());
            let replay = chat(State(h.clone()), headers.clone(), Json(failed_input()))
                .await
                .unwrap()
                .0;
            assert_eq!(replay, result);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            drop(h);
            let reopened = Hub::open(failure_config(path.to_str().unwrap(), runtime)).unwrap();
            let observed = status(
                State(reopened.clone()),
                headers.clone(),
                Path("failure-fixture".into()),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(observed["status"], "failed");
            assert_eq!(history(&reopened, "alice").unwrap()[0].status, "failed");
            assert_eq!(
                chat(State(reopened), headers, Json(failed_input()))
                    .await
                    .unwrap()
                    .0,
                result
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("lock"));
        }
    }

    #[tokio::test]
    async fn non_contract_http_failures_remain_unknown_and_never_redispatch() {
        for (response_status, content_type, body) in [
            (
                501,
                "application/json",
                "{\"error\":\"runtime_failed\"}".into(),
            ),
            (
                502,
                "text/html",
                "<html>upstream gateway failed</html>".into(),
            ),
            (502, "application/json", String::new()),
            (
                502,
                "application/json",
                "{\"error\":\"runtime_timeout\"}".into(),
            ),
            (
                502,
                "application/json",
                "{\"error\":\"unknown_failure\"}".into(),
            ),
            (502, "text/html", "{\"error\":\"runtime_failed\"}".into()),
            (
                502,
                "application/json",
                format!(
                    "{{\"error\":\"runtime_failed\",\"details\":\"{}\"}}",
                    "x".repeat(1500)
                ),
            ),
        ] {
            let (h, headers, calls, server) =
                failure_adapter(response_status, content_type, body, ":memory:").await;
            let result = chat(State(h.clone()), headers.clone(), Json(failed_input())).await;
            assert_eq!(result.unwrap_err().0, StatusCode::BAD_GATEWAY);
            let observed = status(
                State(h.clone()),
                headers.clone(),
                Path("failure-fixture".into()),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(observed["status"], "uncertain");
            assert!(observed.get("failure").is_none());
            assert_eq!(history(&h, "alice").unwrap()[0].status, "uncertain");
            assert_eq!(
                chat(State(h), headers, Json(failed_input()))
                    .await
                    .unwrap()
                    .0["turn"]["status"],
                "uncertain"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            server.abort();
        }
    }

    #[tokio::test]
    async fn actual_transport_timeout_marks_message_and_receipt_uncertain_without_retry() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let adapter = Router::new().route(
            "/v1/turn",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<Json<serde_json::Value>>().await
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/turn", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
        let token = "a".repeat(40);
        let mut h = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: vec![UserConfig {
                id: "alice".into(),
                token_sha256: format!("{:x}", Sha256::digest(token.as_bytes())),
            }],
            runtime: Runtime::Dsh {
                url,
                token: "fake-adapter-token".into(),
            },
        })
        .unwrap();
        // Only the fixture shortens the production client's existing 75-second timeout.
        h.client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_millis(100))
            .build()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        let input = || ChatInput {
            message: "hello".into(),
            agent_id: None,
            expected_profile_revision: None,
            request_id: Some("timeout".into()),
            selected_task: None,
            selected_project: None,
        };
        let result = chat(State(h.clone()), headers.clone(), Json(input())).await;
        assert_eq!(result.unwrap_err().0, StatusCode::BAD_GATEWAY);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(history(&h, "alice").unwrap()[0].status, "uncertain");
        let observed = status(State(h.clone()), headers.clone(), Path("timeout".into()))
            .await
            .unwrap()
            .0;
        assert_eq!(observed["status"], "uncertain");
        let replay = chat(State(h), headers, Json(input())).await.unwrap().0;
        assert_eq!(replay["turn"]["status"], "uncertain");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}
