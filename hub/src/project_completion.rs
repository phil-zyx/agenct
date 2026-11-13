//! Explicit human assertions: resolving a blocker and accepting a project are separate actions.
use super::*;
use serde_json::{json, Value};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS project_resolutions (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
 item_id TEXT NOT NULL, item_version INTEGER NOT NULL,
 author TEXT NOT NULL REFERENCES sessions(owner), request_id TEXT NOT NULL,
 action TEXT NOT NULL CHECK(action IN ('resolve','reopen')), note TEXT NOT NULL, account_revision INTEGER NOT NULL,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 FOREIGN KEY(item_id,item_version) REFERENCES project_state_versions(item_id,version),
 UNIQUE(project_id,author,request_id));
CREATE TABLE IF NOT EXISTS project_acceptances (
 id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
 author TEXT NOT NULL REFERENCES sessions(owner), request_id TEXT NOT NULL,
 target_revision INTEGER NOT NULL, accepted_revision INTEGER NOT NULL,
 goals TEXT NOT NULL, tasks TEXT NOT NULL, members TEXT NOT NULL, note TEXT NOT NULL,
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 UNIQUE(project_id,author,request_id));
CREATE TABLE IF NOT EXISTS project_acceptance_withdrawals (
 acceptance_id TEXT PRIMARY KEY REFERENCES project_acceptances(id),
 author TEXT NOT NULL REFERENCES sessions(owner), created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
CREATE TRIGGER IF NOT EXISTS project_resolutions_no_update BEFORE UPDATE ON project_resolutions
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_resolutions_no_delete BEFORE DELETE ON project_resolutions
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_resolutions_no_replace BEFORE INSERT ON project_resolutions
WHEN EXISTS (SELECT 1 FROM project_resolutions WHERE id=NEW.id OR (project_id=NEW.project_id AND author=NEW.author AND request_id=NEW.request_id))
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptances_no_update BEFORE UPDATE ON project_acceptances
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptances_no_delete BEFORE DELETE ON project_acceptances
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptances_no_replace BEFORE INSERT ON project_acceptances
WHEN EXISTS (SELECT 1 FROM project_acceptances WHERE id=NEW.id OR (project_id=NEW.project_id AND author=NEW.author AND request_id=NEW.request_id))
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptance_withdrawals_no_update BEFORE UPDATE ON project_acceptance_withdrawals
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptance_withdrawals_no_delete BEFORE DELETE ON project_acceptance_withdrawals
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
CREATE TRIGGER IF NOT EXISTS project_acceptance_withdrawals_no_replace BEFORE INSERT ON project_acceptance_withdrawals
WHEN EXISTS (SELECT 1 FROM project_acceptance_withdrawals WHERE acceptance_id=NEW.acceptance_id)
BEGIN SELECT RAISE(ABORT,'project completion records are immutable'); END;
";

#[derive(Deserialize, Serialize, PartialEq, Eq, Clone)]
#[serde(deny_unknown_fields)]
pub(super) struct Goal {
    item_id: String,
    item_version: i64,
}
fn key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
}
fn conflict(message: &'static str) -> Error {
    Error(StatusCode::CONFLICT, message)
}
fn account_revision(db: &Connection, who: &str) -> Result<i64> {
    Ok(db.query_row(
        "SELECT COALESCE((SELECT revision FROM member_profiles WHERE owner=?1),0)",
        [who],
        |r| r.get(0),
    )?)
}
fn member_snapshot(db: &Connection, id: &str) -> Result<String> {
    let mut q=db.prepare("SELECT m.owner,COALESCE(p.revision,0),COALESCE(p.status,'active') FROM project_members m LEFT JOIN member_profiles p ON p.owner=m.owner WHERE m.project_id=?1 AND m.status='active' ORDER BY m.owner")?;
    let rows = q
        .query_map([id], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?
            ]))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!(rows).to_string())
}
fn resolution_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"item_id":r.get::<_,String>(1)?,"item_version":r.get::<_,i64>(2)?,"author":r.get::<_,String>(3)?,"action":r.get::<_,String>(4)?,"note":r.get::<_,String>(5)?,"created_at":r.get::<_,String>(6)?,"account_revision":r.get::<_,i64>(7)?}),
    )
}
const RES: &str = "id,item_id,item_version,author,action,note,created_at,account_revision";
struct Check {
    blockers: Vec<Value>,
    goals: Vec<Goal>,
    tasks: Vec<Value>,
    reasons: Vec<&'static str>,
}
fn check(h: &Hub, db: &Connection, p: &projects::Project, id: &str) -> Result<Check> {
    let mut out = Check {
        blockers: vec![],
        goals: vec![],
        tasks: vec![],
        reasons: vec![],
    };
    if projects::writable(p).is_err() {
        out.reasons.push("project_archived");
    }
    let mut q=db.prepare("SELECT i.id,i.created_by,v.version,v.kind,v.status,v.confirmed_by FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND v.version=(SELECT MAX(version) FROM project_state_versions WHERE item_id=i.id) ORDER BY i.id")?;
    let rows = q
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (item, reporter, version, kind, status, confirmed_by) in rows {
        let confirmed = status == "confirmed"
            && match confirmed_by {
                Some(who) => projects::active_member(h, db, p, &who)?,
                None => false,
            };
        if status != "withdrawn" && !confirmed {
            out.reasons.push("unconfirmed_shared_state");
        }
        if kind == "goal" && status != "withdrawn" {
            out.goals.push(Goal {
                item_id: item.clone(),
                item_version: version,
            });
        }
        if kind == "blocker" {
            let mut resolution=db.query_row(&format!("SELECT {RES} FROM project_resolutions WHERE project_id=?1 AND item_id=?2 ORDER BY rowid DESC LIMIT 1"),params![id,item],resolution_row).optional()?;
            let reporter_revision = account_revision(db, &reporter)?;
            let resolved = confirmed
                && projects::active_member(h, db, p, &reporter)?
                && resolution.as_ref().is_some_and(|r| {
                    r["item_version"] == version
                        && r["action"] == "resolve"
                        && r["account_revision"] == reporter_revision
                });
            if let Some(r) = resolution.as_mut() {
                r["valid"] = json!(
                    confirmed
                        && r["item_version"] == version
                        && r["account_revision"] == account_revision(db, &reporter)?
                        && projects::active_member(h, db, p, &reporter)?
                );
                r.as_object_mut().unwrap().remove("account_revision");
            }
            if status != "withdrawn" && !resolved {
                out.reasons.push("unresolved_blocker");
            }
            out.blockers.push(json!({"item_id":item,"item_version":version,"reporter":reporter,"status":status,"resolution":resolution,"resolved":resolved}));
        }
    }
    if out.goals.is_empty() {
        out.reasons.push("no_confirmed_goal");
    }
    // Only mutually shared task links. Never inspect or expose task bodies or pending links.
    let mut q=db.prepare("SELECT t.id,t.revision,t.status,t.requester,t.owner FROM project_tasks l JOIN tasks t ON t.id=l.task_id WHERE l.project_id=?1 AND l.status='active' ORDER BY t.id")?;
    let rows = q
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (task, revision, status, requester, owner) in rows {
        if !projects::active_member(h, db, p, &requester)?
            || !projects::active_member(h, db, p, &owner)?
        {
            out.reasons.push("linked_task_party_unavailable");
            continue;
        }
        if status != "completed" {
            out.reasons.push("linked_task_not_accepted");
        }
        out.tasks
            .push(json!({"task_id":task,"accepted_version":revision,"status":status}));
    }
    out.reasons.sort_unstable();
    out.reasons.dedup();
    Ok(out)
}
fn acceptance_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"author":r.get::<_,String>(1)?,"target_revision":r.get::<_,i64>(2)?,"accepted_revision":r.get::<_,i64>(3)?,"goals":serde_json::from_str::<Value>(&r.get::<_,String>(4)?).unwrap_or(Value::Null),"tasks":serde_json::from_str::<Value>(&r.get::<_,String>(5)?).unwrap_or(Value::Null),"note":r.get::<_,String>(6)?,"created_at":r.get::<_,String>(7)?,"withdrawn":r.get::<_,bool>(8)?,"members":r.get::<_,String>(9)?}),
    )
}
const ACC: &str = "a.id,a.author,a.target_revision,a.accepted_revision,a.goals,a.tasks,a.note,a.created_at,EXISTS(SELECT 1 FROM project_acceptance_withdrawals w WHERE w.acceptance_id=a.id),a.members";
fn token(db: &Connection, p: &projects::Project, id: &str, c: &Check) -> Result<String> {
    Ok(format!(
        "{:x}",
        Sha256::digest(
            json!([
                p.revision,
                member_snapshot(db, id)?,
                c.goals,
                c.tasks,
                c.reasons
            ])
            .to_string()
            .as_bytes()
        )
    ))
}
fn public_acceptance(a: &mut Value) {
    a.as_object_mut().unwrap().remove("members");
    a.as_object_mut().unwrap().remove("tasks");
}
fn validity(
    h: &Hub,
    db: &Connection,
    p: &projects::Project,
    id: &str,
    c: &Check,
    a: &mut Value,
) -> Result<()> {
    a["valid"] = json!(
        a["withdrawn"] == false
            && a["members"] == member_snapshot(db, id)?
            && a["accepted_revision"] == p.revision
            && c.reasons.is_empty()
            && a["goals"] == json!(c.goals)
            && a["tasks"] == json!(c.tasks)
            && projects::active_member(h, db, p, a["author"].as_str().unwrap_or(""))?
    );
    public_acceptance(a);
    if a["withdrawn"] == true {
        a["note"] = json!("");
    }
    Ok(())
}
/// Caller must authorize active project membership while holding the database lock.
pub(super) fn snapshot(h: &Hub, db: &Connection, p: &projects::Project, id: &str) -> Result<Value> {
    let c = check(h, db, p, id)?;
    let mut a=db.query_row(&format!("SELECT {ACC} FROM project_acceptances a WHERE a.project_id=?1 ORDER BY a.rowid DESC LIMIT 1"),[id],acceptance_row).optional()?;
    if let Some(a) = a.as_mut() {
        validity(h, db, p, id, &c, a)?;
    }
    Ok(
        json!({"project_revision":p.revision,"snapshot_token":token(db,p,id,&c)?,"blockers":c.blockers,"goals":c.goals,"tasks":c.tasks,"ready":c.reasons.is_empty(),"reasons":c.reasons,"acceptance":a}),
    )
}
pub(super) async fn list(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    let actor = h.identity(&headers)?;
    let db = h.db()?;
    let p = projects::authorize(&h, &db, &id, &actor)?;
    Ok(Json(snapshot(&h, &db, &p, &id)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Resolve {
    request_id: String,
    expected_snapshot: String,
    expected_revision: i64,
    expected_version: i64,
    action: String,
    note: String,
}
pub(super) async fn resolve(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path((id, item)): Path<(String, String)>,
    Json(input): Json<Resolve>,
) -> Result<Json<Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &id, &actor)?;
    projects::writable(&p)?;
    if !key(&input.request_id)
        || !projects::bounded(&input.note, 4000)
        || !["resolve", "reopen"].contains(&input.action.as_str())
    {
        return Err(invalid("invalid resolution request"));
    }
    let (reporter,version,kind,status): (String,i64,String,String)=tx.query_row("SELECT i.created_by,v.version,v.kind,v.status FROM project_state_items i JOIN project_state_versions v ON v.item_id=i.id WHERE i.project_id=?1 AND i.id=?2 ORDER BY v.version DESC LIMIT 1",params![id,item],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or_else(denied)?;
    if reporter != actor || kind != "blocker" {
        return Err(denied());
    }
    if let Some(mut r)=tx.query_row(&format!("SELECT {RES} FROM project_resolutions WHERE project_id=?1 AND author=?2 AND request_id=?3"),params![id,actor,input.request_id],resolution_row).optional()? {
        if r["item_id"]!=item || r["item_version"]!=input.expected_version || r["action"]!=input.action || r["note"]!=input.note {return Err(conflict("request_id already used for different resolution"));}
        let latest:String=tx.query_row("SELECT id FROM project_resolutions WHERE project_id=?1 AND item_id=?2 ORDER BY rowid DESC LIMIT 1",params![id,item],|r|r.get(0))?;
        r["valid"]=json!(r["id"]==latest && r["item_version"]==version && status=="confirmed" && r["account_revision"]==account_revision(&tx,&actor)?);
        r.as_object_mut().unwrap().remove("account_revision");
        return Ok(Json(json!({"project_revision":p.revision,"resolution":r,"replayed":true})));
    }
    if input.expected_snapshot != token(&tx, &p, &id, &check(&h, &tx, &p, &id)?)? {
        return Err(conflict("project preview changed; reload"));
    }
    if version != input.expected_version || status != "confirmed" {
        return Err(conflict("blocker changed or is not confirmed"));
    }
    let previous:Option<(i64,String,i64)>=tx.query_row("SELECT item_version,action,account_revision FROM project_resolutions WHERE project_id=?1 AND item_id=?2 ORDER BY rowid DESC LIMIT 1",params![id,item],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let ar = account_revision(&tx, &actor)?;
    let resolved = previous.is_some_and(|(v, a, r)| v == version && a == "resolve" && r == ar);
    if (input.action == "resolve") == resolved {
        return Err(conflict("blocker already has this resolution state"));
    }
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    let rid = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO project_resolutions(id,project_id,item_id,item_version,author,request_id,action,note,account_revision) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![rid,id,item,version,actor,input.request_id,input.action,input.note,ar])?;
    let mut r = tx.query_row(
        &format!("SELECT {RES} FROM project_resolutions WHERE id=?1"),
        [rid],
        resolution_row,
    )?;
    r.as_object_mut().unwrap().remove("account_revision");
    r["valid"] = json!(true);
    tx.commit()?;
    Ok(Json(
        json!({"project_revision":revision,"resolution":r,"replayed":false}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Accept {
    request_id: String,
    expected_snapshot: String,
    expected_revision: i64,
    goals: Vec<Goal>,
    tasks: Vec<Value>,
    note: String,
}
pub(super) async fn accept(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Accept>,
) -> Result<Json<Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &id, &actor)?;
    projects::writable(&p)?;
    if actor != p.creator {
        return Err(denied());
    }
    if !key(&input.request_id) || !projects::bounded(&input.note, 4000) {
        return Err(invalid("invalid acceptance request"));
    }
    let c = check(&h, &tx, &p, &id)?;
    if let Some(mut a)=tx.query_row(&format!("SELECT {ACC} FROM project_acceptances a WHERE a.project_id=?1 AND a.author=?2 AND a.request_id=?3"),params![id,actor,input.request_id],acceptance_row).optional()? {
        if a["target_revision"]!=input.expected_revision || a["goals"]!=json!(input.goals) || a["tasks"]!=json!(input.tasks) || a["note"]!=input.note {return Err(conflict("request_id already used for different acceptance"));}
        validity(&h,&tx,&p,&id,&c,&mut a)?;
        return Ok(Json(json!({"project_revision":p.revision,"acceptance":a,"replayed":true})));
    }
    if input.expected_snapshot != token(&tx, &p, &id, &c)? {
        return Err(conflict("project preview changed; reload"));
    }
    if !c.reasons.is_empty() || input.goals != c.goals || input.tasks != c.tasks {
        return Err(conflict("project is not ready or goal versions changed"));
    }
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    let aid = Uuid::new_v4().to_string();
    tx.execute("INSERT INTO project_acceptances(id,project_id,author,request_id,target_revision,accepted_revision,goals,tasks,members,note) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![aid,id,actor,input.request_id,input.expected_revision,revision,json!(c.goals).to_string(),json!(c.tasks).to_string(),member_snapshot(&tx,&id)?,input.note])?;
    let mut a = tx.query_row(
        &format!("SELECT {ACC} FROM project_acceptances a WHERE a.id=?1"),
        [aid],
        acceptance_row,
    )?;
    a["valid"] = json!(true);
    public_acceptance(&mut a);
    tx.commit()?;
    Ok(Json(
        json!({"project_revision":revision,"acceptance":a,"replayed":false}),
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
    Path((id, aid)): Path<(String, String)>,
    Json(input): Json<Withdraw>,
) -> Result<Json<Value>> {
    let actor = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let p = projects::authorize(&h, &tx, &id, &actor)?;
    // Archiving prevents new commitments, not retraction of an existing assertion.
    let a = tx
        .query_row(
            &format!("SELECT {ACC} FROM project_acceptances a WHERE a.project_id=?1 AND a.id=?2"),
            params![id, aid],
            acceptance_row,
        )
        .optional()?
        .ok_or_else(denied)?;
    if a["author"] != actor {
        return Err(denied());
    }
    if a["withdrawn"] == true {
        return Ok(Json(json!({"project_revision":p.revision})));
    }
    let revision = projects::bump(&tx, &p, input.expected_revision)?;
    tx.execute(
        "INSERT INTO project_acceptance_withdrawals(acceptance_id,author) VALUES (?1,?2)",
        params![aid, actor],
    )?;
    tx.commit()?;
    Ok(Json(json!({"project_revision":revision})))
}
#[cfg(test)]
#[path = "project_completion_tests.rs"]
mod tests;
