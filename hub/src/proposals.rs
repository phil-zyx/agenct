//! Sender-private proposals and explicit confirmation before task creation.
use crate::{denied, invalid, Error, Hub, Result};
use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Serialize)]
pub(super) struct Proposal {
    id: String,
    requester: String,
    owner: String,
    summary: String,
    status: String,
    revision: i64,
    task_id: Option<String>,
}
fn row_proposal(r: &rusqlite::Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: r.get(0)?,
        requester: r.get(1)?,
        owner: r.get(2)?,
        summary: r.get(3)?,
        status: r.get(4)?,
        revision: r.get(5)?,
        task_id: r.get(6)?,
    })
}
const PROPOSAL_COLUMNS: &str = "id,requester,owner,summary,status,revision,task_id";
fn save_proposal(
    h: &Hub,
    requester: &str,
    owner: &str,
    summary: &str,
    request_id: Option<&str>,
    source: &str,
) -> Result<Proposal> {
    if owner == requester || !h.gates.contains_key(owner) {
        return Err(invalid("select another configured participant"));
    }
    if summary.trim().is_empty() || summary.len() > 4000 {
        return Err(invalid("summary must contain 1..4000 bytes"));
    }
    let mut db = h.db()?;
    if let Some(key) = request_id {
        let collision: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM chat_turns WHERE owner=?1 AND request_id=?2)",
            params![requester, key],
            |r| r.get(0),
        )?;
        if collision {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used by chat endpoint",
            ));
        }
    }
    let tx = db.transaction()?;
    let proposal = save_proposal_tx(&tx, requester, owner, summary, request_id, source)?;
    tx.commit()?;
    Ok(proposal)
}
pub(super) fn save_proposal_tx(
    tx: &rusqlite::Transaction<'_>,
    requester: &str,
    owner: &str,
    summary: &str,
    request_id: Option<&str>,
    source: &str,
) -> Result<Proposal> {
    let generated = Uuid::new_v4().to_string();
    let key = request_id.unwrap_or(&generated);
    if key.is_empty()
        || key.len() > 128
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(invalid("invalid request_id"));
    }
    let existing = tx.query_row(&format!("SELECT {PROPOSAL_COLUMNS},source FROM proposals WHERE requester=?1 AND request_id=?2"),params![requester,key],|r|Ok((row_proposal(r)?,r.get::<_,String>(7)?))).optional()?;
    if let Some((proposal, previous)) = existing {
        if previous != source || proposal.owner != owner || proposal.summary != summary {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used for different content",
            ));
        }
        return Ok(proposal);
    }
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO proposals(id,requester,owner,summary,request_id,source) VALUES (?1,?2,?3,?4,?5,?6)",params![id,requester,owner,summary,key,source])?;
    let p = tx.query_row(
        &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1"),
        [id],
        row_proposal,
    )?;
    Ok(p)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProposalInput {
    owner: String,
    summary: String,
    request_id: Option<String>,
}
pub(super) async fn propose(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(p): Json<ProposalInput>,
) -> Result<Json<Proposal>> {
    let user = h.identity(&headers)?;
    Ok(Json(save_proposal(
        &h,
        &user,
        &p.owner,
        &p.summary,
        p.request_id.as_deref(),
        "explicit",
    )?))
}
pub(super) async fn proposals(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Vec<Proposal>>> {
    let user = h.identity(&headers)?;
    let db = h.db()?;
    let mut q = db.prepare(&format!(
        "SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE requester=?1 ORDER BY rowid DESC"
    ))?;
    let rows = q.query_map([user], row_proposal)?;
    Ok(Json(rows.collect::<std::result::Result<Vec<_>, _>>()?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProposalAction {
    action: String,
    expected_revision: i64,
}
pub(super) async fn proposal_action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<ProposalAction>,
) -> Result<Json<Proposal>> {
    let user = h.identity(&headers)?;
    if a.action != "confirm" && a.action != "reject" {
        return Err(invalid("unsupported proposal action"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = tx
        .query_row(
            &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1 AND requester=?2"),
            params![id, user],
            row_proposal,
        )
        .optional()?
        .ok_or_else(denied)?;
    // Retry of the identical action at the reviewed revision returns the same result.
    // This cannot authorize different content: proposal fields are immutable.
    let target = if a.action == "confirm" {
        "confirmed"
    } else {
        "rejected"
    };
    if p.status == target && a.expected_revision == 0 && p.revision == 1 {
        return Ok(Json(p));
    }
    if p.status != "pending" || p.revision != a.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "proposal changed; refresh and review",
        ));
    }
    if a.action == "confirm" && (!h.gates.contains_key(&p.owner) || p.owner == user) {
        return Err(invalid("recipient is no longer available"));
    }
    let task_id = if a.action == "confirm" {
        let task = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO tasks(id,requester,owner,summary,status) VALUES (?1,?2,?3,?4,'awaiting_owner')",params![task,user,p.owner,p.summary])?;
        tx.execute(
            "INSERT INTO events(task_id,actor,action) VALUES (?1,?2,'created')",
            params![task, user],
        )?;
        Some(task)
    } else {
        None
    };
    tx.execute(
        "UPDATE proposals SET status=?1,revision=revision+1,task_id=?2 WHERE id=?3",
        params![target, task_id, id],
    )?;
    let result = tx.query_row(
        &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1"),
        [id],
        row_proposal,
    )?;
    tx.commit()?;
    Ok(Json(result))
}
