//! Opt-in project coordination. Membership never widens existing task/chat ACLs.
use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
 id TEXT PRIMARY KEY, creator TEXT NOT NULL REFERENCES sessions(owner),
 name TEXT NOT NULL, goal TEXT NOT NULL, kind TEXT NOT NULL CHECK(kind IN ('temporary','ongoing')),
 status TEXT NOT NULL DEFAULT 'active' CHECK(status IN ('active','archived')),
 revision INTEGER NOT NULL DEFAULT 0 CHECK(revision>=0), request_id TEXT NOT NULL,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, UNIQUE(creator,request_id));
CREATE TABLE IF NOT EXISTS project_members (
 project_id TEXT NOT NULL REFERENCES projects(id), owner TEXT NOT NULL REFERENCES sessions(owner),
 status TEXT NOT NULL CHECK(status IN ('invited','active','left','removed','declined')),
 PRIMARY KEY(project_id,owner));
CREATE TABLE IF NOT EXISTS project_tasks (
 project_id TEXT NOT NULL REFERENCES projects(id), task_id TEXT NOT NULL REFERENCES tasks(id),
 label TEXT NOT NULL, proposed_by TEXT NOT NULL REFERENCES sessions(owner),
 status TEXT NOT NULL CHECK(status IN ('pending','active','revoked')),
 PRIMARY KEY(project_id,task_id));
CREATE INDEX IF NOT EXISTS project_members_owner ON project_members(owner,status,project_id);";

#[derive(Serialize)]
pub(super) struct Project {
    id: String,
    creator: String,
    name: String,
    goal: String,
    kind: String,
    status: String,
    revision: i64,
    created_at: String,
}
const COLUMNS: &str = "id,creator,name,goal,kind,status,revision,created_at";
fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: r.get(0)?,
        creator: r.get(1)?,
        name: r.get(2)?,
        goal: r.get(3)?,
        kind: r.get(4)?,
        status: r.get(5)?,
        revision: r.get(6)?,
        created_at: r.get(7)?,
    })
}
fn read(db: &Connection, id: &str) -> Result<Project> {
    db.query_row(
        &format!("SELECT {COLUMNS} FROM projects WHERE id=?1"),
        [id],
        row,
    )
    .optional()?
    .ok_or_else(denied)
}
fn bounded(value: &str, limit: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= limit
        && !value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}
fn account_active(h: &Hub, db: &Connection, user: &str) -> Result<bool> {
    Ok(h.directory.contains(user)
        && db.query_row(
            "SELECT COALESCE((SELECT status='active' FROM member_profiles WHERE owner=?1),1)",
            [user],
            |r| r.get::<_, bool>(0),
        )?)
}
fn membership(db: &Connection, project: &str, user: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT status FROM project_members WHERE project_id=?1 AND owner=?2",
            params![project, user],
            |r| r.get(0),
        )
        .optional()?)
}
fn same_workspace(h: &Hub, db: &Connection, p: &Project, user: &str) -> Result<bool> {
    Ok(account_active(h, db, user)?
        && account_active(h, db, &p.creator)?
        && (p.creator == user || h.directory.can_delegate(&p.creator, user)))
}
fn active_member(h: &Hub, db: &Connection, p: &Project, user: &str) -> Result<bool> {
    Ok(
        same_workspace(h, db, p, user)?
            && membership(db, &p.id, user)?.as_deref() == Some("active"),
    )
}
fn authorize(h: &Hub, db: &Connection, id: &str, user: &str) -> Result<Project> {
    let p = read(db, id)?;
    if !active_member(h, db, &p, user)? {
        return Err(denied());
    }
    Ok(p)
}
// Withdrawing one's own consent must remain possible while the creator is
// disabled or no longer configured. This grants no project content access.
fn withdrawal_member(h: &Hub, db: &Connection, p: &Project, user: &str) -> Result<()> {
    if !account_active(h, db, user)? || membership(db, &p.id, user)?.as_deref() != Some("active") {
        return Err(denied());
    }
    Ok(())
}
fn owner(p: &Project, user: &str) -> Result<()> {
    if p.creator != user {
        return Err(denied());
    }
    Ok(())
}
fn writable(p: &Project) -> Result<()> {
    if p.status != "active" {
        return Err(Error(StatusCode::CONFLICT, "project is archived"));
    }
    Ok(())
}
fn bump(db: &Connection, p: &Project, expected: i64) -> Result<i64> {
    if expected != p.revision || expected == i64::MAX {
        return Err(Error(
            StatusCode::CONFLICT,
            "project changed; reload before acting",
        ));
    }
    let revision = expected + 1;
    if db.execute(
        "UPDATE projects SET revision=?1 WHERE id=?2 AND revision=?3",
        params![revision, p.id, expected],
    )? != 1
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "project changed; reload before acting",
        ));
    }
    Ok(revision)
}
fn receipt(revision: i64) -> Json<serde_json::Value> {
    Json(serde_json::json!({"revision":revision}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Create {
    name: String,
    goal: String,
    kind: String,
    request_id: String,
}
pub(super) async fn create(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Create>,
) -> Result<Json<Project>> {
    let actor = h.identity(&headers)?;
    if !bounded(&input.name, 120)
        || !bounded(&input.goal, 4000)
        || !["temporary", "ongoing"].contains(&input.kind.as_str())
        || input.request_id.is_empty()
        || input.request_id.len() > 128
        || !input
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(invalid("invalid project name, goal, kind, or request_id"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    if let Some(p) = tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM projects WHERE creator=?1 AND request_id=?2"),
            params![actor, input.request_id],
            row,
        )
        .optional()?
    {
        if p.name != input.name || p.goal != input.goal || p.kind != input.kind {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used for different content",
            ));
        }
        return Ok(Json(p));
    }
    let id = Uuid::new_v4().to_string();
    tx.execute(
        "INSERT INTO projects(id,creator,name,goal,kind,request_id) VALUES (?1,?2,?3,?4,?5,?6)",
        params![
            id,
            actor,
            input.name,
            input.goal,
            input.kind,
            input.request_id
        ],
    )?;
    tx.execute(
        "INSERT INTO project_members(project_id,owner,status) VALUES (?1,?2,'active')",
        params![id, actor],
    )?;
    let project = read(&tx, &id)?;
    tx.commit()?;
    Ok(Json(project))
}

pub(super) async fn list(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Project>>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let mut q = db.prepare("SELECT project_id FROM project_members WHERE owner=?1 AND status='active' ORDER BY rowid DESC")?;
    let ids = q
        .query_map([&actor], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut projects = Vec::new();
    for id in ids {
        let p = read(&db, &id)?;
        if active_member(&h, &db, &p, &actor)? {
            projects.push(p);
        }
    }
    Ok(Json(projects))
}
// Invitation preview is intentionally separate: no roster, links or task history.
pub(super) async fn invitations(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Vec<Project>>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let mut q = db.prepare("SELECT project_id FROM project_members WHERE owner=?1 AND status='invited' ORDER BY rowid DESC")?;
    let ids = q
        .query_map([&actor], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut projects = Vec::new();
    for id in ids {
        let p = read(&db, &id)?;
        if p.status == "active" && same_workspace(&h, &db, &p, &actor)? {
            projects.push(p);
        }
    }
    Ok(Json(projects))
}
pub(super) async fn detail(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = authorize(&h, &db, &id, &actor)?;
    let mut q =
        db.prepare("SELECT owner,status FROM project_members WHERE project_id=?1 ORDER BY owner")?;
    let rows = q
        .query_map([&id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut members = Vec::new();
    for (user, status) in rows {
        if same_workspace(&h, &db, &p, &user)? && (status == "active" || actor == p.creator) {
            members.push(serde_json::json!({"owner":user,"status":status}));
        }
    }
    // Explicit projection. Never SELECT task summaries, results, materials, events,
    // private messages, drafts, model profiles or worker artifacts here.
    let mut q = db.prepare("SELECT l.task_id,l.label,l.proposed_by,l.status,t.requester,t.owner,t.status,t.revision FROM project_tasks l JOIN tasks t ON t.id=l.task_id WHERE l.project_id=?1 AND l.status!='revoked' ORDER BY l.rowid")?;
    let rows = q
        .query_map([&id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, i64>(7)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tasks = Vec::new();
    for (task_id, label, proposed_by, status, requester, task_owner, task_status, task_revision) in
        rows
    {
        if !active_member(&h, &db, &p, &requester)? || !active_member(&h, &db, &p, &task_owner)? {
            continue;
        }
        if status == "active" || actor == requester || actor == task_owner {
            tasks.push(serde_json::json!({"task_id":task_id,"label":label,"proposed_by":proposed_by,"status":status,
                "requester":requester,"owner":task_owner,"task_status":task_status,"task_revision":task_revision,
                "can_open_task":actor==requester || actor==task_owner}));
        }
    }
    Ok(Json(
        serde_json::json!({"project":p,"members":members,"tasks":tasks}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Invite {
    owner: String,
    expected_revision: i64,
}
pub(super) async fn invite(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Invite>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = authorize(&h, &tx, &id, &actor)?;
    owner(&p, &actor)?;
    writable(&p)?;
    if input.owner == actor || !same_workspace(&h, &tx, &p, &input.owner)? {
        return Err(denied());
    }
    if membership(&tx, &id, &input.owner)?
        .as_deref()
        .is_some_and(|s| s == "active" || s == "invited")
    {
        return Err(Error(
            StatusCode::CONFLICT,
            "member is already active or invited",
        ));
    }
    let count: i64 = tx.query_row("SELECT count(*) FROM project_members WHERE project_id=?1 AND status IN ('active','invited')",[&id],|r|r.get(0))?;
    if count >= 50 {
        return Err(invalid(
            "project supports at most 50 active or invited members",
        ));
    }
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute("INSERT INTO project_members(project_id,owner,status) VALUES (?1,?2,'invited') ON CONFLICT(project_id,owner) DO UPDATE SET status='invited'",params![id,input.owner])?;
    tx.commit()?;
    Ok(receipt(revision))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    action: String,
    expected_revision: i64,
}
fn revoke_member_links(db: &Connection, project: &str, user: &str) -> Result<()> {
    db.execute("UPDATE project_tasks SET status='revoked' WHERE project_id=?1 AND task_id IN (SELECT id FROM tasks WHERE requester=?2 OR owner=?2)",params![project,user])?;
    Ok(())
}
pub(super) async fn membership_status(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = read(&db, &id)?;
    withdrawal_member(&h, &db, &p, &actor)?;
    Ok(Json(
        serde_json::json!({"status":"active","revision":p.revision}),
    ))
}
pub(super) async fn change_membership(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Action>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = read(&tx, &id)?;
    if input.action == "leave" {
        withdrawal_member(&h, &tx, &p, &actor)?;
    } else if !same_workspace(&h, &tx, &p, &actor)? {
        return Err(denied());
    }
    let current = membership(&tx, &id, &actor)?;
    let next = match (input.action.as_str(), current.as_deref()) {
        ("accept", Some("invited")) => {
            writable(&p)?;
            "active"
        }
        ("decline", Some("invited")) => "declined",
        ("leave", Some("active")) if actor != p.creator => "left",
        _ => return Err(denied()),
    };
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute(
        "UPDATE project_members SET status=?1 WHERE project_id=?2 AND owner=?3",
        params![next, id, actor],
    )?;
    if next != "active" {
        revoke_member_links(&tx, &id, &actor)?;
    }
    tx.commit()?;
    Ok(receipt(revision))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Revision {
    expected_revision: i64,
}
pub(super) async fn remove_member(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((id, member)): Path<(String, String)>,
    Json(input): Json<Revision>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = authorize(&h, &tx, &id, &actor)?;
    owner(&p, &actor)?;
    if member == actor
        || !membership(&tx, &id, &member)?
            .as_deref()
            .is_some_and(|s| s == "active" || s == "invited")
    {
        return Err(denied());
    }
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute(
        "UPDATE project_members SET status='removed' WHERE project_id=?1 AND owner=?2",
        params![id, member],
    )?;
    revoke_member_links(&tx, &id, &member)?;
    tx.commit()?;
    Ok(receipt(revision))
}
pub(super) async fn archive(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Revision>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = authorize(&h, &tx, &id, &actor)?;
    owner(&p, &actor)?;
    writable(&p)?;
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute("UPDATE projects SET status='archived' WHERE id=?1", [&id])?;
    tx.commit()?;
    Ok(receipt(revision))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Link {
    task_id: String,
    label: String,
    expected_revision: i64,
}
fn participants(db: &Connection, task: &str) -> Result<(String, String)> {
    db.query_row(
        "SELECT requester,owner FROM tasks WHERE id=?1",
        [task],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()?
    .ok_or_else(denied)
}
pub(super) async fn link_task(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Link>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    if !bounded(&input.label, 240) {
        return Err(invalid("shared task label must contain 1..240 characters"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = authorize(&h, &tx, &id, &actor)?;
    writable(&p)?;
    let (requester, task_owner) = participants(&tx, &input.task_id)?;
    if (actor != requester && actor != task_owner)
        || !active_member(&h, &tx, &p, &requester)?
        || !active_member(&h, &tx, &p, &task_owner)?
    {
        return Err(denied());
    }
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM project_tasks WHERE project_id=?1 AND task_id=?2",
            params![id, input.task_id],
            |r| r.get(0),
        )
        .optional()?;
    if status.as_deref().is_some_and(|s| s != "revoked") {
        return Err(Error(
            StatusCode::CONFLICT,
            "task link already proposed or active",
        ));
    }
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute("INSERT INTO project_tasks(project_id,task_id,label,proposed_by,status) VALUES (?1,?2,?3,?4,'pending') ON CONFLICT(project_id,task_id) DO UPDATE SET label=excluded.label,proposed_by=excluded.proposed_by,status='pending'",params![id,input.task_id,input.label,actor])?;
    tx.commit()?;
    Ok(receipt(revision))
}
pub(super) async fn link_action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((id, task)): Path<(String, String)>,
    Json(input): Json<Action>,
) -> Result<Json<serde_json::Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = read(&tx, &id)?;
    if input.action == "revoke" {
        withdrawal_member(&h, &tx, &p, &actor)?;
    } else {
        authorize(&h, &tx, &id, &actor)?;
    }
    let (proposer, status): (String, String) = tx
        .query_row(
            "SELECT proposed_by,status FROM project_tasks WHERE project_id=?1 AND task_id=?2",
            params![id, task],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .ok_or_else(denied)?;
    let (requester, task_owner) = participants(&tx, &task)?;
    let participant = actor == requester || actor == task_owner;
    let next = match input.action.as_str() {
        "approve" if status == "pending" && participant && actor != proposer => {
            writable(&p)?;
            if !active_member(&h, &tx, &p, &requester)? || !active_member(&h, &tx, &p, &task_owner)?
            {
                return Err(denied());
            }
            "active"
        }
        "revoke"
            if status != "revoked"
                && (participant || (actor == p.creator && status == "active")) =>
        {
            "revoked"
        }
        _ => return Err(denied()),
    };
    let revision = bump(&tx, &p, input.expected_revision)?;
    tx.execute(
        "UPDATE project_tasks SET status=?1 WHERE project_id=?2 AND task_id=?3",
        params![next, id, task],
    )?;
    tx.commit()?;
    Ok(receipt(revision))
}

/// Enabling tenant isolation cannot adopt a previously cross-tenant project.
pub(super) fn audit_tenants(db: &Connection) -> std::result::Result<(), &'static str> {
    let invalid: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM project_members m JOIN projects p ON p.id=m.project_id LEFT JOIN tenant_memberships a ON a.owner=p.creator LEFT JOIN tenant_memberships b ON b.owner=m.owner WHERE a.tenant IS NULL OR b.tenant IS NULL OR a.tenant!=b.tenant)",[],|r|r.get(0)).map_err(|_|"project tenant startup audit failed")?;
    if invalid {
        return Err("existing projects cross tenant boundaries or reference unmapped users");
    }
    Ok(())
}

#[cfg(test)]
#[path = "projects_tests.rs"]
mod tests;
