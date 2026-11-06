//! Owner-private message history and mock/adapter chat execution.
use crate::{
    chat_turns, invalid, profiles,
    proposals::save_proposal_tx,
    runtime::{adapter_failed, DshRuntime, MockRuntime, Runtime as RuntimeBackend, TurnRequest},
    task_context, Hub, Result, Runtime,
};
use axum::{extract::State, http::HeaderMap, Json};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub status: String,
}
pub(super) fn history(h: &Hub, owner: &str) -> Result<Vec<Message>> {
    let db = h.db()?;
    let mut stmt=db.prepare("SELECT role,content,status FROM (SELECT id,role,content,status FROM messages WHERE owner=?1 ORDER BY id DESC LIMIT 40) ORDER BY id")?;
    let rows = stmt.query_map([owner], |r| {
        Ok(Message {
            role: r.get(0)?,
            content: r.get(1)?,
            status: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}
pub(super) async fn messages(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Vec<Message>>> {
    let id = h.identity(&headers)?;
    Ok(Json(history(&h, &id)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ChatInput {
    pub(super) message: String,
    pub(super) agent_id: Option<String>,
    pub(super) expected_profile_revision: Option<i64>,
    pub(super) request_id: Option<String>,
    pub(super) selected_task: Option<task_context::Selection>,
}
pub(super) async fn execute_chat(
    h: &Hub,
    id: &str,
    input: &ChatInput,
    runtime_profile: profiles::RuntimeProfile,
    task_context: Option<serde_json::Value>,
) -> Result<Json<serde_json::Value>> {
    let prior: Vec<_> = history(h, id)?
        .into_iter()
        .filter(|m| m.status == "complete")
        .collect();
    // Generate the test double's reply without DB access. Preserve its established
    // proposal-only history behavior, but never bypass shared output validation.
    let mock_answer = if matches!(h.runtime, Runtime::Mock) {
        Some(
            MockRuntime
                .turn(TurnRequest {
                    kind: crate::runtime::TurnKind::Chat,
                    message: &input.message,
                    session_id: "",
                    history: &prior,
                    profile: &runtime_profile,
                    task_context: task_context.as_ref(),
                })
                .await?,
        )
    } else {
        None
    };
    let proposal_only = mock_answer
        .as_ref()
        .is_some_and(|answer| answer.proposal.is_some());
    let (message_id, session_id) = if proposal_only {
        (None, String::new())
    } else {
        let mut db = h.db()?;
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO messages(owner,role,content,status) VALUES (?1,'user',?2,'pending')",
            params![id, input.message],
        )?;
        let message_id = tx.last_insert_rowid();
        if let Some(key) = input.request_id.as_deref() {
            tx.execute("UPDATE chat_turns SET message_id=?3 WHERE owner=?1 AND request_id=?2 AND status='pending'", params![id,key,message_id])?;
        }
        let sid: String = tx.query_row(
            "SELECT session_id FROM sessions WHERE owner=?1",
            [id],
            |r| r.get(0),
        )?;
        tx.commit()?;
        (Some(message_id), sid)
    };
    let answer = match (&h.runtime, mock_answer) {
        (Runtime::Mock, Some(answer)) => Ok(answer),
        (Runtime::Dsh { url, token }, _) => {
            DshRuntime {
                client: &h.client,
                url,
                token,
            }
            .turn(TurnRequest {
                kind: crate::runtime::TurnKind::Chat,
                message: &input.message,
                session_id: &session_id,
                history: &prior,
                profile: &runtime_profile,
                task_context: task_context.as_ref(),
            })
            .await
        }
        _ => unreachable!("mock reply is generated above"),
    }
    .and_then(|answer| {
        if answer.valid(h.runtime.label(), id, &input.message, |owner| {
            h.directory.can_delegate(id, owner)
        }) {
            Ok(answer)
        } else if matches!(h.runtime, Runtime::Mock) {
            Err(invalid(
                "select another configured participant and bounded summary",
            ))
        } else {
            Err(adapter_failed())
        }
    });
    let mut db = h.db()?;
    let tx = db.transaction()?;
    match answer {
        Ok(answer) => {
            // Model output is untrusted. Only an immutable, sender-private proposal
            // is persisted here; the existing authenticated confirm action creates tasks.
            let proposal = match answer
                .proposal
                .as_ref()
                .map(|p| {
                    save_proposal_tx(
                        &tx,
                        id,
                        &p.owner,
                        &p.summary,
                        input.request_id.as_deref(),
                        &input.message,
                    )
                })
                .transpose()
            {
                Ok(p) => p,
                Err(error) => {
                    tx.execute(
                        "UPDATE messages SET status='failed' WHERE id=?1",
                        [message_id],
                    )?;
                    tx.commit()?;
                    return Err(error);
                }
            };
            let reply = answer.reply;
            tx.execute(
                "UPDATE messages SET status='complete' WHERE id=?1",
                [message_id],
            )?;
            if message_id.is_some() {
                tx.execute("INSERT INTO messages(owner,role,content,status) VALUES (?1,'assistant',?2,'complete')",params![id,reply])?;
            }
            let mut response = serde_json::json!({"reply":reply,"backend":h.runtime.label(),"proposal":proposal,"runtime_profile":runtime_profile,"task_context":task_context});
            chat_turns::complete(&tx, id, input.request_id.as_deref(), &mut response)?;
            tx.commit()?;
            Ok(Json(response))
        }
        Err(error) => {
            tx.execute(
                "UPDATE messages SET status='failed' WHERE id=?1",
                [message_id],
            )?;
            tx.commit()?;
            Err(error)
        }
    }
}
