//! Durable, owner-private at-most-once dispatch receipts, not provider exactly-once execution.
use super::*;

pub(super) fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "BEGIN IMMEDIATE; CREATE TABLE IF NOT EXISTS chat_turns (
        owner TEXT NOT NULL, request_id TEXT NOT NULL, message TEXT NOT NULL,
        agent_id TEXT, expected_profile_revision INTEGER, runtime_profile TEXT NOT NULL,
        status TEXT NOT NULL CHECK(status IN ('pending','uncertain','complete')),
        response TEXT, message_id INTEGER, selected_task TEXT, task_context TEXT, PRIMARY KEY(owner,request_id));
        UPDATE messages SET status='uncertain' WHERE status IN ('pending','interrupted') AND id IN (SELECT message_id FROM chat_turns WHERE status='pending');
        UPDATE chat_turns SET status='uncertain' WHERE status='pending'; COMMIT;",
    )?;
    let columns = db
        .prepare("PRAGMA table_info(chat_turns)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    for column in ["selected_task", "task_context"] {
        if !columns.iter().any(|name| name == column) {
            db.execute(
                &format!("ALTER TABLE chat_turns ADD COLUMN {column} TEXT"),
                [],
            )?;
        }
    }
    if !columns.iter().any(|name| name == "expected_profile_revision") {
        db.execute(
            "ALTER TABLE chat_turns ADD COLUMN expected_profile_revision INTEGER",
            [],
        )?;
    }
    Ok(())
}
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
        Ok(serde_json::json!({"request_id":key,"status":status,"response":response}))
    })
    .transpose()
}
fn replay(db: &Connection, owner: &str, input: &ChatInput) -> Result<Option<serde_json::Value>> {
    let Some(key) = input.request_id.as_deref() else {
        return Ok(None);
    };
    let binding = db
        .query_row(
            "SELECT message,agent_id,selected_task,expected_profile_revision FROM chat_turns WHERE owner=?1 AND request_id=?2",
            params![owner, key],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<i64>>(3)?)),
        )
        .optional()?;
    let Some((message, agent_id, selected_task, expected_profile_revision)) = binding else {
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
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "request_id already used for different content, agent, profile revision or selected task",
        ));
    }
    let found = receipt(db, owner, key)?.ok_or_else(denied)?;
    if found["status"] == "complete" {
        return Ok(Some(found["response"].clone()));
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
    if let Some(response) = replay(&*h.db()?, &id, &input)? {
        return Ok(Json(response));
    }
    let _guard = h
        .gates
        .get(&id)
        .unwrap()
        .try_lock()
        .map_err(|_| Error(StatusCode::CONFLICT, "a turn is already running"))?;
    if let Some(response) = replay(&*h.db()?, &id, &input)? {
        return Ok(Json(response));
    }
    let (runtime_profile, task_context) = {
        // Capture and reserve under the shared profile/task mutation mutex.
        // Later edits cannot alter the frozen routing or discussion snapshot.
        let db = h.db()?;
        let runtime_profile = profiles::route(
            &db,
            &id,
            input.agent_id.as_deref(),
            input.expected_profile_revision,
        )?;
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
            db.execute("INSERT INTO chat_turns(owner,request_id,message,agent_id,runtime_profile,status,selected_task,task_context,expected_profile_revision) VALUES (?1,?2,?3,?4,?5,'pending',?6,?7,?8)",
                params![id,key,input.message,input.agent_id,serde_json::to_string(&runtime_profile).map_err(|_| invalid("invalid runtime profile"))?,selection,task_context.as_ref().map(|c|c.to_string()),input.expected_profile_revision])?;
        }
        (runtime_profile, task_context)
    };
    let result = execute_chat(&h, &id, &input, runtime_profile, task_context).await;
    if result.is_err() {
        // A timeout/error cannot prove the provider never acted. Never retry dispatch.
        if let Some(key) = input.request_id.as_deref() {
            mark_uncertain(&mut *h.db()?, &id, key)?;
        }
    }
    result
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
    if let Ok(_guard) = h.gates.get(&id).unwrap().try_lock() {
        mark_uncertain(&mut *h.db()?, &id, &key)?;
    }
    Ok(Json(receipt(&*h.db()?, &id, &key)?.ok_or_else(denied)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
