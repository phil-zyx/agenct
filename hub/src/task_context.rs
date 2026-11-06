//! Read-only snapshots of one explicitly selected, participant-visible shared task.
use super::tasks::row_task;
use super::*;
pub(super) const STALE: &str = "selected task changed; refresh and select it again";
pub(super) const UNAVAILABLE: &str = "selected task is not available for discussion";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Selection {
    pub(super) task_id: String,
    pub(super) expected_revision: i64,
}
impl Selection {
    pub(super) fn valid(&self) -> bool {
        valid_id(&self.task_id, 128) && self.expected_revision >= 0
    }
}
fn valid_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}
/// Bound the JSON-encoded field, including escaped control characters, without
/// splitting UTF-8. The explicit flag distinguishes an excerpt from full content.
fn excerpt(value: &str, budget: usize) -> (String, bool) {
    let mut used = 2;
    let mut output = String::new();
    for ch in value.chars() {
        let cost = serde_json::to_string(&ch.to_string())
            .expect("character JSON")
            .len()
            - 2;
        if used + cost > budget {
            break;
        }
        used += cost;
        output.push(ch);
    }
    let truncated = output.len() < value.len();
    (output, truncated)
}
pub(super) fn capture(
    db: &Connection,
    user: &str,
    selection: Option<&Selection>,
) -> Result<Option<serde_json::Value>> {
    let Some(selection) = selection else {
        return Ok(None);
    };
    let task = db.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",
        params![selection.task_id,user],row_task).optional()?.ok_or(Error(StatusCode::NOT_FOUND,UNAVAILABLE))?;
    if task.revision != selection.expected_revision {
        return Err(Error(StatusCode::CONFLICT, STALE));
    }
    if !valid_id(&task.id, 128)
        || !valid_id(&task.requester, 64)
        || !valid_id(&task.owner, 64)
        || ![
            "awaiting_owner",
            "working",
            "ready_for_acceptance",
            "completed",
            "canceled",
        ]
        .contains(&task.status.as_str())
    {
        return Err(Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid stored task context",
        ));
    }
    let (summary, summary_truncated) = excerpt(&task.summary, 2000);
    let (result, result_truncated) = match &task.result {
        Some(value) => {
            let (text, cut) = excerpt(value, 5000);
            (Some(text), cut)
        }
        None => (None, false),
    };
    let context = serde_json::json!({"id":task.id,"revision":task.revision,"requester":task.requester,
        "owner":task.owner,"status":task.status,"summary":summary,"result":result,
        "summary_truncated":summary_truncated,"result_truncated":result_truncated});
    if context.to_string().len() > 8192 {
        return Err(invalid("selected task context exceeds limit"));
    }
    Ok(Some(context))
}
