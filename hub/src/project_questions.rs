//! Explicit member-published feedback, never consensus or an instruction to an agent.
use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_questions (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
 item_id TEXT NOT NULL, item_version INTEGER NOT NULL,
 author TEXT NOT NULL REFERENCES sessions(owner), request_id TEXT NOT NULL,
 content TEXT NOT NULL, reply_to TEXT REFERENCES project_questions(id),
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 FOREIGN KEY(item_id,item_version) REFERENCES project_state_versions(item_id,version),
 UNIQUE(project_id,author,request_id));
CREATE TABLE IF NOT EXISTS project_question_withdrawals (
 message_id TEXT PRIMARY KEY REFERENCES project_questions(id),
 author TEXT NOT NULL REFERENCES sessions(owner), created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
CREATE TRIGGER IF NOT EXISTS project_questions_no_update BEFORE UPDATE ON project_questions
BEGIN SELECT RAISE(ABORT,'project feedback is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_questions_no_delete BEFORE DELETE ON project_questions
BEGIN SELECT RAISE(ABORT,'project feedback is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_questions_no_replace BEFORE INSERT ON project_questions
WHEN EXISTS (SELECT 1 FROM project_questions WHERE id=NEW.id OR
 (project_id=NEW.project_id AND author=NEW.author AND request_id=NEW.request_id))
BEGIN SELECT RAISE(ABORT,'project feedback is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_question_withdrawals_no_update BEFORE UPDATE ON project_question_withdrawals
BEGIN SELECT RAISE(ABORT,'feedback withdrawal is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_question_withdrawals_no_delete BEFORE DELETE ON project_question_withdrawals
BEGIN SELECT RAISE(ABORT,'feedback withdrawal is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_question_withdrawals_no_replace BEFORE INSERT ON project_question_withdrawals
WHEN EXISTS (SELECT 1 FROM project_question_withdrawals WHERE message_id=NEW.message_id)
BEGIN SELECT RAISE(ABORT,'feedback withdrawal is immutable'); END;";

#[derive(Serialize)]
struct Message {
    id: String,
    item_id: String,
    item_version: i64,
    author: String,
    content: String,
    reply_to: Option<String>,
    created_at: String,
    withdrawn: bool,
}
const COLUMNS: &str = "q.id,q.item_id,q.item_version,q.author,q.content,q.reply_to,q.created_at,EXISTS(SELECT 1 FROM project_question_withdrawals w WHERE w.message_id=q.id)";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        item_id: r.get(1)?,
        item_version: r.get(2)?,
        author: r.get(3)?,
        content: r.get(4)?,
        reply_to: r.get(5)?,
        created_at: r.get(6)?,
        withdrawn: r.get(7)?,
    })
}
fn redact(mut message: Message) -> Message {
    if message.withdrawn {
        message.content.clear();
    }
    message
}
fn source(db: &Connection, project: &str, item: &str) -> Result<(i64, String)> {
    db.query_row("SELECT v.version,v.status FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND i.id=?2 ORDER BY v.version DESC LIMIT 1", params![project,item], |r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or_else(denied)
}
fn read(db: &Connection, project: &str, item: &str, id: &str) -> Result<Message> {
    db.query_row(&format!("SELECT {COLUMNS} FROM project_questions q WHERE q.project_id=?1 AND q.item_id=?2 AND q.id=?3"),params![project,item,id],row).optional()?.ok_or_else(denied)
}
/// Called only after project membership authorization by the context capturer.
/// Preserve reference provenance, but never restore withdrawn feedback bodies.
pub(super) fn snapshot(db: &Connection, project: &str) -> Result<Vec<serde_json::Value>> {
    let mut q = db.prepare(&format!("SELECT {COLUMNS},v.version,v.status,EXISTS(SELECT 1 FROM project_question_withdrawals w WHERE w.message_id=q.reply_to) FROM project_questions q JOIN project_state_versions v ON v.item_id=q.item_id AND v.version=(SELECT MAX(version) FROM project_state_versions WHERE item_id=q.item_id) WHERE q.project_id=?1 ORDER BY q.rowid"))?;
    let messages = q
        .query_map([project], |r| {
            let message = redact(row(r)?);
            let current_version: i64 = r.get(8)?;
            let item_status: String = r.get(9)?;
            let parent_withdrawn: bool = r.get(10)?;
            let mut value = serde_json::to_value(&message).expect("feedback serializes");
            value["current_item_version"] = serde_json::json!(current_version);
            value["item_status"] = serde_json::json!(item_status);
            value["current_reference"] = serde_json::json!(
                message.item_version == current_version && item_status != "withdrawn"
            );
            value["parent_withdrawn"] = serde_json::json!(parent_withdrawn);
            Ok(value)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(messages)
}
pub(super) async fn list(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((project, item)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = projects::authorize(&h, &db, &project, &actor)?;
    let (version, status) = source(&db, &project, &item)?;
    let mut q=db.prepare(&format!("SELECT {COLUMNS} FROM project_questions q WHERE q.project_id=?1 AND q.item_id=?2 ORDER BY q.rowid"))?;
    let messages = q
        .query_map(params![project, item], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(redact)
        .collect::<Vec<_>>();
    Ok(Json(
        serde_json::json!({"project_revision":p.revision,"item_version":version,"item_status":status,"messages":messages}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    request_id: String,
    expected_revision: i64,
    expected_version: i64,
    content: String,
    reply_to: Option<String>,
}
pub(super) async fn create(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((project, item)): Path<(String, String)>,
    Json(input): Json<Create>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &project, &actor)?;
    let (version, status) = source(&tx, &project, &item)?;
    if input.request_id.is_empty()
        || input.request_id.len() > 128
        || !input
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
        || !projects::bounded(&input.content, 4000)
    {
        return Err(invalid("invalid feedback content or request_id"));
    }
    let existing=tx.query_row(&format!("SELECT {COLUMNS} FROM project_questions q WHERE q.project_id=?1 AND q.author=?2 AND q.request_id=?3"),params![project,actor,input.request_id],row).optional()?;
    if let Some(message) = existing {
        if message.item_id != item
            || message.item_version != input.expected_version
            || message.content != input.content
            || message.reply_to != input.reply_to
        {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used for different feedback",
            ));
        }
        return Ok(Json(
            serde_json::json!({"project_revision":p.revision,"message":redact(message)}),
        ));
    }
    projects::writable(&p)?;
    if version != input.expected_version || status == "withdrawn" {
        return Err(Error(
            StatusCode::CONFLICT,
            "shared source changed; reload before publishing feedback",
        ));
    }
    if let Some(parent) = &input.reply_to {
        let root = read(&tx, &project, &item, parent)?;
        if root.item_version != version || root.reply_to.is_some() || root.withdrawn {
            return Err(Error(
                StatusCode::CONFLICT,
                "question is not open for replies",
            ));
        }
    }
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    let id = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO project_questions(id,project_id,item_id,item_version,author,request_id,content,reply_to) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![id,project,item,version,actor,input.request_id,input.content,input.reply_to])?;
    let message = read(&tx, &project, &item, &id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"project_revision":revision,"message":message}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Withdraw {
    expected_revision: i64,
}
pub(super) async fn withdraw(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((project, item, id)): Path<(String, String, String)>,
    Json(input): Json<Withdraw>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &project, &actor)?;
    let message = read(&tx, &project, &item, &id)?;
    if message.author != actor {
        return Err(denied());
    }
    if message.withdrawn {
        return Ok(Json(
            serde_json::json!({"project_revision":p.revision,"message":redact(message)}),
        ));
    }
    // Withdrawal is allowed on old source versions and archived projects.
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    tx.execute(
        "INSERT INTO project_question_withdrawals(message_id,author) VALUES (?1,?2)",
        params![id, actor],
    )?;
    let message = read(&tx, &project, &item, &id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"project_revision":revision,"message":redact(message)}),
    ))
}

#[cfg(test)]
#[path = "project_questions_tests.rs"]
mod tests;
