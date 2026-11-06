//! Owner-private message history and mock/adapter chat execution.
use crate::{
    chat_turns, invalid, profiles, proposals::save_proposal_tx, task_context, Error, Hub, Result,
    Runtime,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterProposal {
    owner: String,
    summary: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterReply {
    reply: String,
    backend: String,
    // Explicit null means no proposal; an omitted field is a protocol error.
    #[serde(deserialize_with = "Option::deserialize")]
    proposal: Option<AdapterProposal>,
}
impl AdapterReply {
    fn valid(&self, h: &Hub, requester: &str, message: &str) -> bool {
        self.backend == "dsh"
            && !self.reply.trim().is_empty()
            && self.reply.len() <= 32000
            && self.proposal.as_ref().is_none_or(|p| {
                p.owner != requester
                    && h.gates.contains_key(&p.owner)
                    && message.contains(&p.owner)
                    && !p.summary.trim().is_empty()
                    && p.summary.len() <= 4000
                    && message.contains(&p.summary)
            })
    }
}
pub(super) async fn execute_chat(
    h: &Hub,
    id: &str,
    input: &ChatInput,
    runtime_profile: profiles::RuntimeProfile,
    task_context: Option<serde_json::Value>,
) -> Result<Json<serde_json::Value>> {
    // The deterministic mock is a test double, not a model or DSH tool call.
    // No prior/private history is consulted when constructing the shared summary.
    if matches!(h.runtime, Runtime::Mock) {
        if let Some((owner, summary)) = mock_delegation(&input.message) {
            if owner == id
                || !h.gates.contains_key(&owner)
                || summary.trim().is_empty()
                || summary.len() > 4000
            {
                return Err(invalid(
                    "select another configured participant and bounded summary",
                ));
            }
            let mut db = h.db()?;
            let tx = db.transaction()?;
            let proposal = save_proposal_tx(
                &tx,
                id,
                &owner,
                &summary,
                input.request_id.as_deref(),
                &input.message,
            )?;
            let mut response = serde_json::json!({
                "reply":"[MOCK · deterministic test double] 请检查接收人和将共享的任务内容，再明确确认。确认前对方不会收到任务。",
                "backend":"mock", "proposal":proposal, "runtime_profile":runtime_profile,"task_context":task_context
            });
            chat_turns::complete(&tx, id, input.request_id.as_deref(), &mut response)?;
            tx.commit()?;
            return Ok(Json(response));
        }
    }
    let prior: Vec<_> = history(h, id)?
        .into_iter()
        .filter(|m| m.status == "complete")
        .collect();
    let (message_id, session_id) = {
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
        (message_id, sid)
    };
    let answer=match &h.runtime {
        Runtime::Mock=>Ok(AdapterReply {reply: format!("[MOCK · no model] 收到你的消息。当前已保存 {} 条已完成上下文消息。真实任务请使用对话中的委派操作；本回复不会执行工具。",prior.len()), backend:"mock".into(), proposal:None}),
        Runtime::Dsh{url,token}=>{
            let mut payload=serde_json::json!({"session_id":session_id,"runtime_profile":runtime_profile,"history":prior.iter().map(|m|serde_json::json!({"role":m.role,"content":m.content})).collect::<Vec<_>>(),"message":input.message});
            if let Some(context) = &task_context {payload["task_context"] = context.clone();}
            while payload["history"].as_array().unwrap().len()>100 || serde_json::to_vec(&payload).unwrap().len()>60000 {
                if payload["history"].as_array().unwrap().is_empty() {return Err(invalid("chat context exceeds adapter limit"));}
                payload["history"].as_array_mut().unwrap().remove(0);
            }
            let response=h.client.post(url).bearer_auth(token).json(&payload).send().await;
            match response {
                Ok(r) if r.status().is_success()=>match r.json::<AdapterReply>().await {Ok(r) if r.valid(h,id,&input.message)=>Ok(r),_=>Err(())},
                _=>Err(()),
            }
        }
    };
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
            tx.execute("INSERT INTO messages(owner,role,content,status) VALUES (?1,'assistant',?2,'complete')",params![id,reply])?;
            let mut response = serde_json::json!({"reply":reply,"backend":h.runtime.label(),"proposal":proposal,"runtime_profile":runtime_profile,"task_context":task_context});
            chat_turns::complete(&tx, id, input.request_id.as_deref(), &mut response)?;
            tx.commit()?;
            Ok(Json(response))
        }
        Err(()) => {
            tx.execute(
                "UPDATE messages SET status='failed' WHERE id=?1",
                [message_id],
            )?;
            tx.commit()?;
            Err(Error(
                StatusCode::BAD_GATEWAY,
                "adapter failed; no mock fallback; task actions were not executed",
            ))
        }
    }
}

// Intentionally bounded recognition for the offline acceptance fixture. DSH replies
// remain plain text and have no task-creation authority.
fn mock_delegation(message: &str) -> Option<(String, String)> {
    let text = message.trim();
    let rest = text
        .strip_prefix("请让 ")
        .or_else(|| text.strip_prefix("ask "))?;
    let (owner, rest) = rest.split_once(' ')?;
    let summary = rest
        .strip_prefix("帮我")
        .map(str::trim_start)
        .or_else(|| rest.strip_prefix("to "))?;
    Some((owner.to_owned(), summary.to_owned()))
}
