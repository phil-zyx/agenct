//! Sender-private proposals and explicit confirmation before task creation.
use crate::{denied, invalid, policy, Error, Hub, Result};
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
    /// UTC "YYYY-MM-DD HH:MM:SS"; None for proposals written before schema v11.
    created_at: Option<String>,
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
        created_at: r.get(7)?,
    })
}
const PROPOSAL_COLUMNS: &str = "id,requester,owner,summary,status,revision,task_id,created_at";
fn save_proposal(
    h: &Hub,
    requester: &str,
    owner: &str,
    summary: &str,
    request_id: Option<&str>,
    source: &str,
) -> Result<Proposal> {
    if !h.directory.can_delegate(requester, owner) || !h.member_active(owner)? {
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
    let existing = tx.query_row(&format!("SELECT {PROPOSAL_COLUMNS},source FROM proposals WHERE requester=?1 AND request_id=?2"),params![requester,key],|r|Ok((row_proposal(r)?,r.get::<_,String>(8)?))).optional()?;
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
    tx.execute("INSERT INTO proposals(id,requester,owner,summary,request_id,source,created_at) VALUES (?1,?2,?3,?4,?5,?6,CURRENT_TIMESTAMP)",params![id,requester,owner,summary,key,source])?;
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
    /// Optional requester-written material shared with the recipient on confirm.
    #[serde(default)]
    details: Option<String>,
    /// Explicit requester edits, applied atomically with confirmation.
    owner: Option<String>,
    summary: Option<String>,
}
pub(super) const MAX_DETAILS: usize = 262144;
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
    if a.details
        .as_ref()
        .is_some_and(|d| a.action != "confirm" || d.trim().is_empty() || d.len() > MAX_DETAILS)
    {
        return Err(invalid(
            "shared details must be 1..262144 bytes and only accompany confirm",
        ));
    }
    if (a.owner.is_some() || a.summary.is_some()) && a.action != "confirm" {
        return Err(invalid("proposal edits may only accompany confirm"));
    }
    if a.summary
        .as_ref()
        .is_some_and(|s| s.trim().is_empty() || s.len() > 4000)
    {
        return Err(invalid("summary must contain 1..4000 bytes"));
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
    let owner = a.owner.as_deref().unwrap_or(&p.owner);
    let summary = a.summary.as_deref().unwrap_or(&p.summary);
    let policy::Effect::Proposal {
        status: target,
        replay,
    } = policy::can(
        &user,
        policy::Action::Proposal(&a.action),
        &policy::Resource::Proposal {
            requester: &p.requester,
            owner,
            status: &p.status,
            revision: p.revision,
            expected_revision: a.expected_revision,
            recipient_available: h.directory.can_delegate(&user, owner) && tx.query_row(
                "SELECT COALESCE((SELECT status='active' FROM member_profiles WHERE owner=?1),1)", [owner], |row| row.get::<_, bool>(0))?,
        },
    )
    .map_err(Error::from)?
    else {
        unreachable!("proposal decision")
    };
    if replay {
        if a.action == "confirm" {
            if owner != p.owner || summary != p.summary {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "proposal already confirmed with different edits",
                ));
            }
            // A replay must restate exactly what was shared; never silently differ.
            let stored: Option<String> = tx
                .query_row(
                    "SELECT content FROM task_details WHERE task_id=?1",
                    [&p.task_id],
                    |r| r.get(0),
                )
                .optional()?;
            if stored != a.details {
                return Err(Error(
                    StatusCode::CONFLICT,
                    "proposal already confirmed with different shared details",
                ));
            }
        }
        return Ok(Json(p));
    }
    let task_id = if a.action == "confirm" {
        let task = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO tasks(id,requester,owner,summary,status) VALUES (?1,?2,?3,?4,'awaiting_owner')",params![task,user,owner,summary])?;
        crate::audit::record(&tx, &task, &user, "created", None, "awaiting_owner", 0)?;
        if let Some(details) = &a.details {
            tx.execute(
                "INSERT INTO task_details(task_id,author,content) VALUES (?1,?2,?3)",
                params![task, user, details],
            )?;
        }
        Some(task)
    } else {
        None
    };
    tx.execute(
        "UPDATE proposals SET status=?1,revision=revision+1,task_id=?2,owner=?4,summary=?5 WHERE id=?3",
        params![target, task_id, id, owner, summary],
    )?;
    let result = tx.query_row(
        &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1"),
        [id],
        row_proposal,
    )?;
    tx.commit()?;
    Ok(Json(result))
}
