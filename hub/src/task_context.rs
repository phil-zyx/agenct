//! Read-only snapshots of one explicitly selected, participant-visible shared task.
use super::tasks::row_task;
use super::*;
pub(super) const STALE: &str = "selected task changed; refresh and select it again";
pub(super) const UNAVAILABLE: &str = "selected task is not available for discussion";
/// Must match the adapter's MAX_TASK_CONTEXT_BYTES.
pub(super) const MAX_CONTEXT_BYTES: usize = 20480;
/// Bound on cached shared-material summaries; cleared wholesale when full.
const MATERIAL_SUMMARY_CACHE: usize = 256;

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
    let stored: Option<String> = db
        .query_row(
            "SELECT content FROM task_details WHERE task_id=?1",
            [&task.id],
            |r| r.get(0),
        )
        .optional()?;
    let (details, details_truncated) = match &stored {
        Some(value) => {
            let (text, cut) = excerpt(value, 12000);
            (Some(text), cut)
        }
        None => (None, false),
    };
    let context = serde_json::json!({"id":task.id,"revision":task.revision,"requester":task.requester,
        "owner":task.owner,"status":task.status,"summary":summary,"result":result,
        "details":details,"summary_truncated":summary_truncated,"result_truncated":result_truncated,
        "details_truncated":details_truncated});
    if context.to_string().len() > MAX_CONTEXT_BYTES {
        return Err(invalid("selected task context exceeds limit"));
    }
    Ok(Some(context))
}

/// Replace an excerpt of large shared material with a bounded semantic summary.
/// Only explicitly shared task_details are read; no private conversations enter it.
pub(super) async fn prepare_large_details(
    h: &Hub,
    user: &str,
    profile: &profiles::RuntimeProfile,
    mut context: serde_json::Value,
) -> Result<serde_json::Value> {
    if context["details_truncated"] != true {
        return Ok(context);
    }
    let task = context["id"]
        .as_str()
        .ok_or_else(|| invalid("invalid selected task"))?
        .to_owned();
    let revision = context["revision"]
        .as_i64()
        .ok_or_else(|| invalid("invalid selected task revision"))?;
    let selection = Selection {
        task_id: task.clone(),
        expected_revision: revision,
    };
    let material = {
        let db = h.db()?;
        capture(&db, user, Some(&selection))?;
        db.query_row(
            "SELECT content FROM task_details WHERE task_id=?1",
            [&task],
            |r| r.get::<_, String>(0),
        )?
    };
    // Shared material is append-only, so (caller, task, backend, profile, content)
    // fully determines the summary input. Reuse it rather than re-summarizing every
    // chunk of the material on every turn. Keyed per caller: a profile is private.
    let key = {
        let mut hasher = Sha256::new();
        for part in [
            user,
            task.as_str(),
            h.runtime.label(),
            &serde_json::to_string(profile).map_err(|_| invalid("invalid runtime profile"))?,
            material.as_str(),
        ] {
            hasher.update(part.len().to_le_bytes());
            hasher.update(part.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    };
    let cached = h
        .material_summaries
        .lock()
        .map_err(|_| invalid("summary cache unavailable"))?
        .get(&key)
        .cloned();
    let summary = match cached {
        Some(summary) => summary,
        None => {
            let summary = crate::context_memory::summarize(
                h,
                profile,
                "",
                &[Message {
                    role: "user".into(),
                    content: material,
                    status: "complete".into(),
                    created_at: None,
                }],
            )
            .await?;
            let mut cache = h
                .material_summaries
                .lock()
                .map_err(|_| invalid("summary cache unavailable"))?;
            if cache.len() >= MATERIAL_SUMMARY_CACHE {
                cache.clear();
            }
            cache.insert(key, summary.clone());
            summary
        }
    };
    context["details"] = serde_json::json!(format!(
        "[共享材料摘要：已读取全部原文生成；摘要可能遗漏细节，核对时请查看原始材料。]\n{summary}"
    ));
    // Keep the existing incomplete/derived-data flag so old clients do not mistake
    // model-generated prose for the full verbatim attachment.
    if context["details"].to_string().len() > 12000 || context.to_string().len() > MAX_CONTEXT_BYTES
    {
        return Err(invalid("shared material summary exceeds context limit"));
    }
    capture(&*h.db()?, user, Some(&selection))?;
    Ok(context)
}
