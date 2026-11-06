//! Per-user "work index": one entry per unit of work (a delegated task or one of
//! the caller's own unconfirmed/rejected proposals), with its latest step and time.
//!
//! Read-only and derived from existing records under the same visibility rules:
//! participants see shared task events; private draft activity is included only
//! for the task owner who generated it; proposals are sender-private.
use crate::{Hub, Result};
use axum::{extract::State, http::HeaderMap, Json};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

const LIMIT: usize = 100;

#[derive(Serialize)]
pub(super) struct Latest {
    /// created | accept | deliver | approve | return | cancel | draft_complete |
    /// draft_uncertain | draft_pending | proposal_pending | proposal_rejected
    action: String,
    actor: String,
    /// UTC "YYYY-MM-DD HH:MM:SS"; None when written before timestamps existed.
    at: Option<String>,
    /// Shared cancellation reason only; never private text.
    reason: Option<String>,
}
#[derive(Serialize)]
pub(super) struct Item {
    kind: &'static str,
    id: String,
    title: String,
    status: String,
    role: &'static str,
    counterpart: String,
    latest: Latest,
}

pub(super) async fn activity(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Item>>> {
    let user = h.identity(&headers)?;
    let db = h.db()?;
    let mut items = Vec::new();

    let mut tasks = db.prepare(
        "SELECT id,requester,owner,summary,status,revision FROM tasks WHERE requester=?1 OR owner=?1",
    )?;
    let rows = tasks
        .query_map([&user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, requester, owner, summary, status, revision) in rows {
        // Same participant/action filter as the shared audit timeline.
        let shared = db
            .query_row(
                "SELECT e.action,e.actor,e.at,CASE WHEN e.action='cancel' THEN e.reason ELSE NULL END
                 FROM events e JOIN tasks t ON t.id=e.task_id
                 WHERE e.task_id=?1 AND e.actor IN (t.requester,t.owner)
                 AND e.action IN ('created','accept','deliver','approve','return','cancel')
                 ORDER BY e.id DESC LIMIT 1",
                [&id],
                |r| {
                    Ok(Latest {
                        action: r.get(0)?,
                        actor: r.get(1)?,
                        at: r.get(2)?,
                        reason: r.get(3)?,
                    })
                },
            )
            .optional()?;
        // Private: only the owner's own draft receipts, never shown to the requester.
        // A draft is the latest step only if it was bound to the task's current
        // revision: every shared transition bumps the revision, so this is exact
        // where second-resolution timestamps would tie.
        let draft = if owner == user {
            db.query_row(
                "SELECT status,created_at FROM draft_turns WHERE owner=?1 AND task_id=?2
                 AND json_extract(binding,'$.expected_revision')=?3
                 ORDER BY rowid DESC LIMIT 1",
                params![user, id, revision],
                |r| {
                    Ok(Latest {
                        action: format!("draft_{}", r.get::<_, String>(0)?),
                        actor: user.clone(),
                        at: r.get(1)?,
                        reason: None,
                    })
                },
            )
            .optional()?
        } else {
            None
        };
        let latest = match (draft, shared) {
            (Some(d), _) => d,
            (None, Some(s)) => s,
            (None, None) => Latest {
                action: "created".into(),
                actor: requester.clone(),
                at: None,
                reason: None,
            },
        };
        let (role, counterpart) = if owner == user {
            ("owner", requester)
        } else {
            ("requester", owner)
        };
        items.push(Item {
            kind: "task",
            id,
            title: summary,
            status,
            role,
            counterpart,
            latest,
        });
    }

    // Confirmed proposals are represented by their task above.
    let mut proposals = db.prepare(
        "SELECT id,owner,summary,status,created_at FROM proposals
         WHERE requester=?1 AND status IN ('pending','rejected')",
    )?;
    let rows = proposals
        .query_map([&user], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, owner, summary, status, at) in rows {
        items.push(Item {
            kind: "proposal",
            id,
            title: summary,
            latest: Latest {
                action: format!("proposal_{status}"),
                actor: user.clone(),
                at,
                reason: None,
            },
            status,
            role: "requester",
            counterpart: owner,
        });
    }

    // Newest first; undated (pre-v11) entries last, in a stable order.
    items.sort_by(|a, b| match (&a.latest.at, &b.latest.at) {
        (Some(x), Some(y)) => y.cmp(x).then_with(|| a.id.cmp(&b.id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.id.cmp(&b.id),
    });
    items.truncate(LIMIT);
    Ok(Json(items))
}
