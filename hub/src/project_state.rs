//! Explicitly published project state, never inferred from private conversations.
use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_state_items (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
 created_by TEXT NOT NULL REFERENCES sessions(owner), request_id TEXT NOT NULL,
 UNIQUE(project_id,created_by,request_id));
CREATE TABLE IF NOT EXISTS project_state_versions (
 item_id TEXT NOT NULL REFERENCES project_state_items(id), version INTEGER NOT NULL CHECK(version>0),
 kind TEXT NOT NULL CHECK(kind IN ('goal','assignment','blocker','decision')),
 content TEXT NOT NULL, assignee TEXT REFERENCES sessions(owner),
 source_kind TEXT NOT NULL CHECK(source_kind IN ('human_statement','model_suggestion')),
 source_note TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('proposed','confirmed','withdrawn')),
 changed_by TEXT NOT NULL REFERENCES sessions(owner), confirmed_by TEXT REFERENCES sessions(owner),
 change TEXT NOT NULL CHECK(change IN ('create','revise','confirm','withdraw','membership_revoked')),
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 CHECK((kind='assignment' AND assignee IS NOT NULL) OR (kind!='assignment' AND assignee IS NULL)),
 CHECK((status='confirmed' AND confirmed_by IS NOT NULL) OR (status!='confirmed' AND confirmed_by IS NULL)),
 PRIMARY KEY(item_id,version));
CREATE TRIGGER IF NOT EXISTS project_state_items_no_update BEFORE UPDATE ON project_state_items
BEGIN SELECT RAISE(ABORT,'project state identity is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_state_items_no_delete BEFORE DELETE ON project_state_items
BEGIN SELECT RAISE(ABORT,'project state identity is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_state_items_no_replace BEFORE INSERT ON project_state_items
WHEN EXISTS (SELECT 1 FROM project_state_items WHERE id=NEW.id OR
 (project_id=NEW.project_id AND created_by=NEW.created_by AND request_id=NEW.request_id))
BEGIN SELECT RAISE(ABORT,'project state identity is immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_state_versions_no_update BEFORE UPDATE ON project_state_versions
BEGIN SELECT RAISE(ABORT,'project state versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS project_state_versions_no_delete BEFORE DELETE ON project_state_versions
BEGIN SELECT RAISE(ABORT,'project state versions are append-only'); END;
CREATE TRIGGER IF NOT EXISTS project_state_versions_no_replace BEFORE INSERT ON project_state_versions
WHEN EXISTS (SELECT 1 FROM project_state_versions WHERE item_id=NEW.item_id AND version=NEW.version)
BEGIN SELECT RAISE(ABORT,'project state versions are append-only'); END;";

#[derive(Serialize)]
pub(super) struct Item {
    id: String,
    project_id: String,
    created_by: String,
    version: i64,
    kind: String,
    content: String,
    assignee: Option<String>,
    source_kind: String,
    source_note: String,
    status: String,
    changed_by: String,
    confirmed_by: Option<String>,
    change: String,
    created_at: String,
    // Confirmation is one named person's assertion, never team consensus.
    // Current eligibility is checked on read, including disabled accounts.
    confirmation_valid: bool,
}
const COLUMNS: &str = "i.id,i.project_id,i.created_by,v.version,v.kind,v.content,v.assignee,v.source_kind,v.source_note,v.status,v.changed_by,v.confirmed_by,v.change,v.created_at";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Item> {
    Ok(Item {
        id: r.get(0)?,
        project_id: r.get(1)?,
        created_by: r.get(2)?,
        version: r.get(3)?,
        kind: r.get(4)?,
        content: r.get(5)?,
        assignee: r.get(6)?,
        source_kind: r.get(7)?,
        source_note: r.get(8)?,
        status: r.get(9)?,
        changed_by: r.get(10)?,
        confirmed_by: r.get(11)?,
        change: r.get(12)?,
        created_at: r.get(13)?,
        confirmation_valid: false,
    })
}
fn latest(db: &Connection, project: &str, item: &str) -> Result<Item> {
    db.query_row(
        &format!("SELECT {COLUMNS} FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND i.id=?2 ORDER BY v.version DESC LIMIT 1"),
        params![project, item], row,
    ).optional()?.ok_or_else(denied)
}
fn confirmation(h: &Hub, db: &Connection, p: &projects::Project, item: &mut Item) -> Result<()> {
    item.confirmation_valid = match &item.confirmed_by {
        Some(who) if item.status == "confirmed" => projects::active_member(h, db, p, who)?,
        _ => false,
    };
    Ok(())
}
fn append(db: &Connection, item: &Item) -> Result<()> {
    db.execute("INSERT INTO project_state_versions(item_id,version,kind,content,assignee,source_kind,source_note,status,changed_by,confirmed_by,change) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![item.id,item.version,item.kind,item.content,item.assignee,item.source_kind,item.source_note,item.status,item.changed_by,item.confirmed_by,item.change])?;
    Ok(())
}
fn next_version(item: &mut Item) -> Result<()> {
    item.version = item
        .version
        .checked_add(1)
        .ok_or_else(|| invalid("state version exhausted"))?;
    Ok(())
}
fn request_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Content {
    kind: String,
    content: String,
    assignee: Option<String>,
    source_kind: String,
    source_note: String,
}
fn validate(h: &Hub, db: &Connection, p: &projects::Project, input: &Content) -> Result<()> {
    if !["goal", "assignment", "blocker", "decision"].contains(&input.kind.as_str())
        || !projects::bounded(&input.content, 4000)
        || !projects::bounded(&input.source_note, 1000)
        || !["human_statement", "model_suggestion"].contains(&input.source_kind.as_str())
        || ((input.kind == "assignment") != input.assignee.is_some())
    {
        return Err(invalid(
            "invalid shared state content, kind, assignee or source",
        ));
    }
    if let Some(assignee) = &input.assignee {
        if !projects::active_member(h, db, p, assignee)? {
            return Err(denied());
        }
    }
    Ok(())
}
fn same_content(item: &Item, input: &Content) -> bool {
    item.kind == input.kind
        && item.content == input.content
        && item.assignee == input.assignee
        && item.source_kind == input.source_kind
        && item.source_note == input.source_note
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    request_id: String,
    expected_revision: i64,
    // Nested to reject unexpected fields (including invented confirmation/source IDs).
    content: Content,
}
pub(super) async fn create(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Create>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &id, &actor)?;
    if !request_key(&input.request_id) {
        return Err(invalid("invalid request_id"));
    }
    if let Some(existing) = tx.query_row("SELECT id FROM project_state_items WHERE project_id=?1 AND created_by=?2 AND request_id=?3",params![id,actor,input.request_id],|r|r.get::<_,String>(0)).optional()? {
        let original = tx.query_row(&format!("SELECT {COLUMNS} FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.id=?1 AND v.version=1"),[&existing],row)?;
        if !same_content(&original, &input.content) {
            return Err(Error(StatusCode::CONFLICT,"request_id already used for different content"));
        }
        let mut item = latest(&tx, &id, &existing)?;
        confirmation(&h, &tx, &p, &mut item)?;
        return Ok(Json(serde_json::json!({"project_revision":p.revision,"item":item})));
    }
    projects::writable(&p)?;
    validate(&h, &tx, &p, &input.content)?;
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    let item_id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO project_state_items(id,project_id,created_by,request_id) VALUES (?1,?2,?3,?4)",
        params![item_id, id, actor, input.request_id],
    )?;
    let item = Item {
        id: item_id,
        project_id: id.clone(),
        created_by: actor.clone(),
        version: 1,
        kind: input.content.kind,
        content: input.content.content,
        assignee: input.content.assignee,
        source_kind: input.content.source_kind,
        source_note: input.content.source_note,
        status: "proposed".into(),
        changed_by: actor,
        confirmed_by: None,
        change: "create".into(),
        created_at: String::new(),
        confirmation_valid: false,
    };
    append(&tx, &item)?;
    let item = latest(&tx, &id, &item.id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"project_revision":revision,"item":item}),
    ))
}
pub(super) async fn list(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = projects::authorize(&h, &db, &id, &actor)?;
    let items = snapshot(&h, &db, &p, &id)?;
    Ok(Json(
        serde_json::json!({"project_revision":p.revision,"items":items}),
    ))
}
pub(super) fn snapshot(
    h: &Hub,
    db: &Connection,
    p: &projects::Project,
    id: &str,
) -> Result<Vec<Item>> {
    let mut q = db.prepare(&format!("SELECT {COLUMNS} FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND v.version=(SELECT max(version) FROM project_state_versions WHERE item_id=i.id) ORDER BY i.rowid"))?;
    let mut items = q
        .query_map([id], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for item in &mut items {
        confirmation(h, db, p, item)?;
    }
    Ok(items)
}

pub(super) async fn history(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((id, item)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = projects::authorize(&h, &db, &id, &actor)?;
    let mut current = latest(&db, &id, &item)?;
    confirmation(&h, &db, &p, &mut current)?;
    let mut q = db.prepare(&format!("SELECT {COLUMNS} FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND i.id=?2 ORDER BY v.version"))?;
    // Historical confirmations are evidence of a past assertion, not a current commitment.
    let versions = q
        .query_map(params![id, item], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(
        serde_json::json!({"project_revision":p.revision,"item":current,"versions":versions}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    action: String,
    expected_revision: i64,
    expected_version: i64,
    content: Option<Content>,
}
pub(super) async fn action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((id, item)): Path<(String, String)>,
    Json(input): Json<Action>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &id, &actor)?;
    projects::writable(&p)?;
    let mut item = latest(&tx, &id, &item)?;
    if item.version != input.expected_version || item.status == "withdrawn" {
        return Err(Error(
            StatusCode::CONFLICT,
            "state changed or withdrawn; reload before acting",
        ));
    }
    let editor = actor == item.created_by || actor == p.creator;
    let can_confirm = match item.kind.as_str() {
        "assignment" => item.assignee.as_deref() == Some(actor.as_str()),
        "blocker" => item.created_by == actor,
        _ => p.creator == actor,
    };
    match input.action.as_str() {
        "revise" => {
            if !editor {
                return Err(denied());
            }
            let content = input
                .content
                .ok_or_else(|| invalid("revision content required"))?;
            validate(&h, &tx, &p, &content)?;
            // A stable item cannot turn into a different kind of claim.
            if content.kind != item.kind {
                return Err(invalid("state kind is immutable"));
            }
            item.content = content.content;
            item.assignee = content.assignee;
            item.source_kind = content.source_kind;
            item.source_note = content.source_note;
            item.status = "proposed".into();
            item.confirmed_by = None;
        }
        "confirm" => {
            if !can_confirm {
                return Err(denied());
            }
            if input.content.is_some() || item.status != "proposed" {
                return Err(invalid(
                    "confirmation requires an unchanged proposed version",
                ));
            }
            item.status = "confirmed".into();
            item.confirmed_by = Some(actor.clone());
        }
        "withdraw" => {
            if !editor && !can_confirm {
                return Err(denied());
            }
            if input.content.is_some() {
                return Err(invalid("withdrawal cannot change content"));
            }
            item.status = "withdrawn".into();
            item.confirmed_by = None;
        }
        _ => return Err(invalid("unknown shared state action")),
    }
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    next_version(&mut item)?;
    item.change = input.action;
    item.changed_by = actor;
    append(&tx, &item)?;
    let mut item = latest(&tx, &id, &item.id)?;
    confirmation(&h, &tx, &p, &mut item)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"project_revision":revision,"item":item}),
    ))
}

// Called in the same transaction as membership withdrawal/removal. Rejoining
// never resurrects old commitments. Published history remains project material.
pub(super) fn revoke_member(
    db: &Connection,
    project: &str,
    member: &str,
    actor: &str,
) -> Result<()> {
    let mut q = db.prepare(&format!("SELECT {COLUMNS} FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND v.version=(SELECT max(version) FROM project_state_versions WHERE item_id=i.id) AND v.status='confirmed' AND v.confirmed_by=?2"))?;
    let items = q
        .query_map(params![project, member], row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for mut item in items {
        next_version(&mut item)?;
        item.status = "proposed".into();
        item.confirmed_by = None;
        item.changed_by = actor.into();
        item.change = "membership_revoked".into();
        append(db, &item)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "project_state_tests.rs"]
mod tests;
