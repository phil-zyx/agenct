//! Member presentation and lifecycle. OIDC subject binding remains operator-owned.
use super::*;
use std::collections::HashSet;
pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS member_profiles(owner TEXT PRIMARY KEY REFERENCES sessions(owner), display_name TEXT NOT NULL, color TEXT NOT NULL DEFAULT 'green', status TEXT NOT NULL DEFAULT 'active', revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS member_invitations(token_hash TEXT PRIMARY KEY, owner TEXT NOT NULL REFERENCES sessions(owner), expires INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS identity_bindings(owner TEXT PRIMARY KEY REFERENCES sessions(owner), issuer TEXT NOT NULL, subject TEXT NOT NULL, UNIQUE(issuer,subject));";
#[derive(Serialize)]
pub struct Member {
    pub id: String,
    pub display_name: String,
    pub color: String,
    pub status: String,
    pub revision: i64,
    pub admin: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    expected_revision: i64,
    display_name: String,
    color: String,
    status: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invite {
    owner: String,
    expected_revision: i64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Accept {
    token: String,
}
fn invalid(message: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, message)
}
impl Hub {
    pub(super) fn restore_gitlab_workspace(
        &mut self,
        issuer: &str,
    ) -> std::result::Result<(), &'static str> {
        let ids = {
            let db = self.db().map_err(|_| "identity storage unavailable")?;
            let unexpected: i64 = db.query_row("SELECT COUNT(*) FROM sessions s LEFT JOIN identity_bindings b ON b.owner=s.owner WHERE s.owner!='login-bootstrap' AND (b.owner IS NULL OR b.issuer!=?1 OR b.owner!='gitlab-'||b.subject)", [issuer], |r|r.get(0)).map_err(|_| "identity audit failed")?;
            if unexpected != 0 {
                return Err("GitLab automatic signup requires an isolated database; legacy accounts cannot be inherited");
            }
            let mut query = db
                .prepare("SELECT owner FROM identity_bindings WHERE issuer=?1")
                .map_err(|_| "identity lookup failed")?;
            let ids = query
                .query_map([issuer], |r| r.get::<_, String>(0))
                .map_err(|_| "identity lookup failed")?
                .collect::<std::result::Result<Vec<_>, _>>()
                .map_err(|_| "identity lookup failed")?;
            ids
        };
        self.directory = Arc::new(crate::directory::Directory::gitlab_workspace(ids));
        Ok(())
    }
    pub(super) fn provision_gitlab_user(&self, issuer: &str, id: &str) -> Result<()> {
        let subject = id
            .strip_prefix("gitlab-")
            .ok_or(invalid("invalid GitLab identity"))?;
        if subject.parse::<u64>().ok().filter(|id| *id > 0).is_none()
            || self.directory.tenants_enabled()
        {
            return Err(invalid("invalid GitLab workspace"));
        }
        let mut db = self.db()?;
        let tx = db.transaction()?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT issuer FROM identity_bindings WHERE owner=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if existing.as_deref().is_some_and(|old| old != issuer) {
            return Err(Error(
                StatusCode::FORBIDDEN,
                "GitLab identity binding mismatch",
            ));
        }
        if existing.is_none() {
            if tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM sessions WHERE owner=?1)",
                [id],
                |r| r.get::<_, bool>(0),
            )? {
                return Err(Error(
                    StatusCode::FORBIDDEN,
                    "existing local account cannot be claimed",
                ));
            }
            tx.execute(
                "INSERT INTO sessions(owner,session_id) VALUES (?1,?2)",
                params![id, Uuid::new_v4().to_string()],
            )?;
            tx.execute(
                "INSERT INTO identity_bindings(owner,issuer,subject) VALUES (?1,?2,?3)",
                params![id, issuer, subject],
            )?;
        }
        tx.commit()?;
        drop(db);
        self.directory.add_gitlab_user(id.into());
        Ok(())
    }

    pub fn with_member_admins(
        mut self,
        ids: Vec<String>,
    ) -> std::result::Result<Self, &'static str> {
        if ids.is_empty() || ids.iter().any(|id| !self.directory.contains(id)) {
            return Err("member administrators must be explicitly configured users");
        }
        self.member_admins = Arc::new(ids.into_iter().collect::<HashSet<_>>());
        Ok(self)
    }
    pub(super) fn member(&self, id: &str) -> Result<Member> {
        let db = self.db()?;
        let value = db
            .query_row(
                "SELECT display_name,color,status,revision FROM member_profiles WHERE owner=?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        let (display_name, color, status, revision) =
            value.unwrap_or((id.into(), "green".into(), "active".into(), 0));
        Ok(Member {
            id: id.into(),
            display_name,
            color,
            status,
            revision,
            admin: self.member_admins.contains(id),
        })
    }
    pub(super) fn member_active(&self, id: &str) -> Result<bool> {
        Ok(self.directory.contains(id) && self.member(id)?.status == "active")
    }
    pub(super) fn bind_subjects(
        &self,
        issuer: &str,
        subjects: &HashMap<String, String>,
    ) -> std::result::Result<(), &'static str> {
        let mut db = self.db().map_err(|_| "identity storage unavailable")?;
        let tx = db
            .transaction()
            .map_err(|_| "identity storage unavailable")?;
        for (subject, id) in subjects {
            let old: Option<(String, String)> = tx
                .query_row(
                    "SELECT issuer,subject FROM identity_bindings WHERE owner=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|_| "identity lookup failed")?;
            if let Some(old) = old {
                if old != (issuer.into(), subject.clone()) {
                    return Err("existing account cannot be rebound to another OIDC identity");
                }
            } else {
                tx.execute(
                    "INSERT INTO identity_bindings(owner,issuer,subject) VALUES (?1,?2,?3)",
                    params![id, issuer, subject],
                )
                .map_err(|_| "OIDC identity already bound")?;
            }
        }
        tx.commit().map_err(|_| "identity binding failed")
    }
}
fn admin(h: &Hub, headers: &HeaderMap) -> Result<String> {
    let id = h.identity(headers)?;
    if !h.member_admins.contains(&id) {
        return Err(Error(StatusCode::FORBIDDEN, "administrator required"));
    }
    Ok(id)
}
fn same_org(h: &Hub, actor: &str, target: &str) -> bool {
    actor == target || h.directory.can_delegate(actor, target)
}
pub async fn list(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Member>>> {
    let actor = admin(&h, &headers)?;
    let mut ids: Vec<_> = h
        .directory
        .ids()
        .filter(|id| same_org(&h, &actor, id))
        .collect();
    ids.sort();
    Ok(Json(
        ids.iter().map(|id| h.member(id)).collect::<Result<_>>()?,
    ))
}
pub async fn update(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(input): Json<Update>,
) -> Result<Json<Member>> {
    let actor = h.identity(&headers)?;
    let is_admin = h.member_admins.contains(&actor);
    if !same_org(&h, &actor, &id) || (actor != id && !is_admin) {
        return Err(Error(StatusCode::FORBIDDEN, "member access denied"));
    }
    let old = h.member(&id)?;
    let name = input.display_name.trim();
    if name.is_empty()
        || name.len() > 96
        || name.chars().any(char::is_control)
        || !["green", "blue", "purple", "orange"].contains(&input.color.as_str())
        || !["active", "disabled", "invited"].contains(&input.status.as_str())
    {
        return Err(invalid("invalid member profile"));
    }
    if input.status != old.status && (!is_admin || actor == id || h.member_admins.contains(&id)) {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "cannot change this member status",
        ));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO member_profiles(owner,display_name) VALUES (?1,?1)",
        [&id],
    )?;
    if tx.execute("UPDATE member_profiles SET display_name=?2,color=?3,status=?4,revision=revision+1 WHERE owner=?1 AND revision=?5",params![id,name,input.color,input.status,input.expected_revision])? != 1 { return Err(Error(StatusCode::CONFLICT,"member changed; reload before editing")); }
    if input.status != old.status {
        tx.execute("DELETE FROM member_invitations WHERE owner=?1", [&id])?;
    }
    tx.commit()?;
    drop(db);
    if input.status != "active" {
        if let Some(auth) = &h.browser_auth {
            auth.revoke_user(&id);
        }
    }
    Ok(Json(h.member(&id)?))
}
pub async fn invite(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Invite>,
) -> Result<Json<serde_json::Value>> {
    let actor = admin(&h, &headers)?;
    if h.browser_auth.is_none() {
        return Err(invalid("invitations require browser OIDC login"));
    }
    if !same_org(&h, &actor, &input.owner)
        || actor == input.owner
        || h.member_admins.contains(&input.owner)
    {
        return Err(Error(StatusCode::FORBIDDEN, "cannot invite this member"));
    }
    if !h
        .browser_auth
        .as_ref()
        .is_some_and(|auth| auth.has_user(&input.owner))
    {
        return Err(invalid("configure the member OIDC subject before inviting"));
    }
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest = format!("{:x}", Sha256::digest(token.as_bytes()));
    let mut db = h.db()?;
    let tx = db.transaction()?;
    if tx.execute("UPDATE member_profiles SET status='invited',revision=revision+1 WHERE owner=?1 AND revision=?2 AND status IN ('disabled','invited')",params![input.owner,input.expected_revision])? != 1 { return Err(Error(StatusCode::CONFLICT,"disable the provisioned account first, or reload its revision")); }
    tx.execute(
        "DELETE FROM member_invitations WHERE owner=?1",
        [&input.owner],
    )?;
    tx.execute(
        "INSERT INTO member_invitations(token_hash,owner,expires) VALUES (?1,?2,unixepoch()+86400)",
        params![digest, input.owner],
    )?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"path":format!("/#invite={token}"),"expires_in":86400}),
    ))
}
pub async fn accept(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Accept>,
) -> Result<Json<serde_json::Value>> {
    if h.browser_auth.is_none() || input.token.len() != 64 {
        return Err(invalid("invalid invitation"));
    }
    let id = h.raw_identity(&headers)?;
    let digest = format!("{:x}", Sha256::digest(input.token.as_bytes()));
    let mut db = h.db()?;
    let tx = db.transaction()?;
    if tx.execute(
        "DELETE FROM member_invitations WHERE token_hash=?1 AND owner=?2 AND expires>unixepoch()",
        params![digest, id],
    )? != 1
    {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "invitation expired, used, or belongs to another identity",
        ));
    }
    if tx.execute("UPDATE member_profiles SET status='active',revision=revision+1 WHERE owner=?1 AND status='invited'",[&id])? != 1 { return Err(Error(StatusCode::CONFLICT,"member is no longer invited")); }
    tx.commit()?;
    Ok(Json(serde_json::json!({"accepted":true})))
}
pub async fn people(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Member>>> {
    let id = h.identity(&headers)?;
    let mut members = Vec::new();
    for other in h.directory.contacts(&id) {
        if h.member_active(&other)? {
            members.push(h.member(&other)?);
        }
    }
    Ok(Json(members))
}
