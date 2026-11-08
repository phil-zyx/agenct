//! Personal collaboration context and authenticated, two-person conversations.
//! Public profile replies are deterministic and never use private context or grant authority.
use super::*;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS collaboration_profiles (
 owner TEXT PRIMARY KEY REFERENCES sessions(owner), revision INTEGER NOT NULL,
 profile_json TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS contact_messages (
 id INTEGER PRIMARY KEY, sender TEXT NOT NULL REFERENCES sessions(owner),
 recipient TEXT NOT NULL REFERENCES sessions(owner),
 author_kind TEXT NOT NULL CHECK(author_kind IN ('human','assistant')),
 body TEXT NOT NULL, request_id TEXT, reply_to INTEGER REFERENCES contact_messages(id),
 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
 UNIQUE(sender,request_id), UNIQUE(reply_to), CHECK(sender!=recipient));
CREATE INDEX IF NOT EXISTS contact_messages_participants ON contact_messages(sender,recipient,id);
CREATE TABLE IF NOT EXISTS contact_reads (
 owner TEXT NOT NULL REFERENCES sessions(owner), contact TEXT NOT NULL REFERENCES sessions(owner),
 through_id INTEGER NOT NULL, PRIMARY KEY(owner,contact));";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicProfile {
    pub team: String,
    pub responsibilities: String,
    pub help_with: String,
    pub request_requirements: String,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PrivateProfile {
    pub work_context: String,
    pub response_preferences: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Permissions {
    pub auto_reply_public: bool,
    pub auto_prepare_drafts: bool,
}
impl Default for Permissions {
    fn default() -> Self {
        Self {
            auto_reply_public: false,
            auto_prepare_drafts: true,
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Profile {
    pub revision: u64,
    pub public: PublicProfile,
    pub private: PrivateProfile,
    pub permissions: Permissions,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Update {
    expected_revision: u64,
    public: PublicProfile,
    private: PrivateProfile,
    permissions: Permissions,
}

pub(super) fn read(db: &Connection, owner: &str) -> Result<Profile> {
    let saved: Option<(u64, String)> = db
        .query_row(
            "SELECT revision,profile_json FROM collaboration_profiles WHERE owner=?1",
            [owner],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match saved {
        None => Ok(Profile::default()),
        Some((revision, raw)) => {
            let profile: Profile = serde_json::from_str(&raw).map_err(|_| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid collaboration profile",
                )
            })?;
            if profile.revision != revision {
                return Err(Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "invalid collaboration revision",
                ));
            }
            Ok(profile)
        }
    }
}
pub(super) async fn get_profile(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Profile>> {
    let owner = h.identity(&headers)?;
    Ok(Json(read(&*h.db()?, &owner)?))
}
fn bounded(value: &str, limit: usize) -> bool {
    value.chars().count() <= limit
        && !value
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
}
pub(super) fn validate_profile(public: &PublicProfile, private: &PrivateProfile) -> Result<()> {
    if !bounded(&public.team, 120)
        || !bounded(&public.responsibilities, 1000)
        || !bounded(&public.help_with, 1000)
        || !bounded(&public.request_requirements, 1000)
        || !bounded(&private.work_context, 2000)
        || !bounded(&private.response_preferences, 1000)
    {
        return Err(invalid(
            "collaboration profile field too long or contains control characters",
        ));
    }
    Ok(())
}
pub(super) fn write(db: &Connection, owner: &str, profile: &Profile) -> Result<()> {
    validate_profile(&profile.public, &profile.private)?;
    let raw =
        serde_json::to_string(profile).map_err(|_| invalid("invalid collaboration profile"))?;
    db.execute("INSERT INTO collaboration_profiles(owner,revision,profile_json) VALUES (?1,?2,?3) ON CONFLICT(owner) DO UPDATE SET revision=excluded.revision,profile_json=excluded.profile_json", params![owner, profile.revision, raw])?;
    Ok(())
}
pub(super) async fn update_profile(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<Update>,
) -> Result<Json<Profile>> {
    let owner = h.identity(&headers)?;
    validate_profile(&input.public, &input.private)?;
    let db = h.db()?;
    let current = read(&db, &owner)?;
    if current.revision != input.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "collaboration profile changed; reload before saving",
        ));
    }
    let revision = current
        .revision
        .checked_add(1)
        .filter(|r| *r <= i64::MAX as u64)
        .ok_or(Error(
            StatusCode::CONFLICT,
            "collaboration revision exhausted",
        ))?;
    let profile = Profile {
        revision,
        public: input.public,
        private: input.private,
        permissions: input.permissions,
    };
    write(&db, &owner, &profile)?;
    Ok(Json(profile))
}

fn authorize(h: &Hub, owner: &str, contact: &str) -> Result<()> {
    if !h.directory.can_delegate(owner, contact) || !h.member_active(contact)? {
        return Err(Error(StatusCode::NOT_FOUND, "contact unavailable"));
    }
    Ok(())
}
pub(super) async fn public_profile(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(contact): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let owner = h.identity(&headers)?;
    authorize(&h, &owner, &contact)?;
    let profile = read(&*h.db()?, &contact)?;
    Ok(Json(
        serde_json::json!({"id":contact,"public":profile.public}),
    ))
}

#[derive(Clone, Serialize)]
pub(super) struct ContactMessage {
    id: i64,
    sender: String,
    recipient: String,
    author_kind: String,
    body: String,
    created_at: String,
}
const MESSAGE_COLUMNS: &str = "id,sender,recipient,author_kind,body,created_at";
fn row_message(r: &rusqlite::Row<'_>) -> rusqlite::Result<ContactMessage> {
    Ok(ContactMessage {
        id: r.get(0)?,
        sender: r.get(1)?,
        recipient: r.get(2)?,
        author_kind: r.get(3)?,
        body: r.get(4)?,
        created_at: r.get(5)?,
    })
}
pub(super) async fn messages(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(contact): Path<String>,
) -> Result<Json<Vec<ContactMessage>>> {
    let owner = h.identity(&headers)?;
    authorize(&h, &owner, &contact)?;
    let db = h.db()?;
    let mut query = db.prepare(&format!("SELECT {MESSAGE_COLUMNS} FROM (SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE (sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1) ORDER BY id DESC LIMIT 100) ORDER BY id"))?;
    let messages = query
        .query_map(params![owner, contact], row_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Json(messages))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Send {
    body: String,
    request_id: String,
}
#[derive(Serialize)]
pub(super) struct Receipt {
    message: ContactMessage,
    reply: Option<ContactMessage>,
}
fn public_reply(profile: &Profile, input: &str) -> Option<String> {
    if !profile.permissions.auto_reply_public {
        return None;
    }
    let text = input.trim().to_lowercase();
    // Deliberately narrow: no generated answer, private memory, commitments, or arbitrary tool action.
    let fields: Vec<(&str, &str)> = if [
        "负责什么",
        "负责哪些",
        "什么职责",
        "职责是什么",
        "what do you do",
        "responsibilities",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        vec![
            ("团队", &profile.public.team),
            ("负责的工作", &profile.public.responsibilities),
        ]
    } else if ["可以帮", "能帮", "找你做什么", "help with"]
        .iter()
        .any(|s| text.contains(s))
    {
        vec![("可以找我协作的事情", &profile.public.help_with)]
    } else if [
        "需要什么材料",
        "需要哪些材料",
        "怎么提需求",
        "协作要求",
        "request requirements",
    ]
    .iter()
    .any(|s| text.contains(s))
    {
        vec![("提出请求时请提供", &profile.public.request_requirements)]
    } else {
        return None;
    };
    let lines: Vec<_> = fields
        .into_iter()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(label, value)| format!("{label}：{value}"))
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "根据本人公开的协作资料：\n{}\n\n由助手自动回复。具体安排需本人确认。",
            lines.join("\n")
        ))
    }
}
pub(super) async fn send(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(contact): Path<String>,
    Json(input): Json<Send>,
) -> Result<Json<Receipt>> {
    let owner = h.identity(&headers)?;
    authorize(&h, &owner, &contact)?;
    if input.body.trim().is_empty() || !bounded(&input.body, 4000) {
        return Err(invalid(
            "message must contain 1..4000 characters without control characters",
        ));
    }
    if input.request_id.is_empty()
        || input.request_id.len() > 128
        || !input
            .request_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err(invalid("invalid request_id"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let prior = tx
        .query_row(
            &format!(
                "SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE sender=?1 AND request_id=?2"
            ),
            params![owner, input.request_id],
            row_message,
        )
        .optional()?;
    if let Some(message) = prior {
        if message.recipient != contact || message.body != input.body {
            return Err(Error(
                StatusCode::CONFLICT,
                "request_id already used for different content",
            ));
        }
        let reply = tx
            .query_row(
                &format!("SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE reply_to=?1"),
                [message.id],
                row_message,
            )
            .optional()?;
        return Ok(Json(Receipt { message, reply }));
    }
    tx.execute("INSERT INTO contact_messages(sender,recipient,author_kind,body,request_id) VALUES (?1,?2,'human',?3,?4)", params![owner,contact,input.body,input.request_id])?;
    let message = tx.query_row(
        &format!("SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE id=?1"),
        [tx.last_insert_rowid()],
        row_message,
    )?;
    let profile = read(&tx, &contact)?;
    let reply = if let Some(body) = public_reply(&profile, &input.body) {
        tx.execute("INSERT INTO contact_messages(sender,recipient,author_kind,body,reply_to) VALUES (?1,?2,'assistant',?3,?4)", params![contact,owner,body,message.id])?;
        Some(tx.query_row(
            &format!("SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE id=?1"),
            [tx.last_insert_rowid()],
            row_message,
        )?)
    } else {
        None
    };
    tx.commit()?;
    Ok(Json(Receipt { message, reply }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Read {
    through_id: i64,
}
pub(super) async fn mark_read(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(contact): Path<String>,
    Json(input): Json<Read>,
) -> Result<Json<serde_json::Value>> {
    let owner = h.identity(&headers)?;
    authorize(&h, &owner, &contact)?;
    let db = h.db()?;
    let belongs: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM contact_messages WHERE id=?3 AND ((sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1)))", params![owner,contact,input.through_id], |r|r.get(0))?;
    if !belongs {
        return Err(invalid(
            "read cursor must reference a message in this conversation",
        ));
    }
    db.execute("INSERT INTO contact_reads(owner,contact,through_id) VALUES (?1,?2,?3) ON CONFLICT(owner,contact) DO UPDATE SET through_id=MAX(through_id,excluded.through_id)", params![owner,contact,input.through_id])?;
    Ok(Json(serde_json::json!({"ok":true})))
}
#[derive(Serialize)]
pub(super) struct Conversation {
    contact_id: String,
    last_message: ContactMessage,
    unread_count: i64,
}
pub(super) async fn conversations(
    State(h): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<Vec<Conversation>>> {
    let owner = h.identity(&headers)?;
    let contacts: Vec<_> = h
        .directory
        .contacts(&owner)
        .into_iter()
        .filter_map(|contact| match h.member_active(&contact) {
            Ok(true) => Some(Ok(contact)),
            Ok(false) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<_>>()?;
    let db = h.db()?;
    let mut result = Vec::new();
    for contact in contacts {
        let last_message = db.query_row(&format!("SELECT {MESSAGE_COLUMNS} FROM contact_messages WHERE (sender=?1 AND recipient=?2) OR (sender=?2 AND recipient=?1) ORDER BY id DESC LIMIT 1"), params![owner,contact], row_message).optional()?;
        if let Some(last_message) = last_message {
            let unread_count = db.query_row("SELECT COUNT(*) FROM contact_messages WHERE sender=?2 AND recipient=?1 AND id>COALESCE((SELECT through_id FROM contact_reads WHERE owner=?1 AND contact=?2),0)", params![owner,contact], |r|r.get(0))?;
            result.push(Conversation {
                contact_id: contact,
                last_message,
                unread_count,
            });
        }
    }
    result.sort_by_key(|item| std::cmp::Reverse(item.last_message.id));
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn all_contact_routes_enforce_tenant_boundaries() {
        let token = "a".repeat(40);
        let mut hub = Hub::open(Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            runtime: Runtime::Mock,
            users: ["alice", "bob", "eve"]
                .into_iter()
                .map(|id| UserConfig {
                    id: id.into(),
                    token_sha256: format!(
                        "{:x}",
                        Sha256::digest(if id == "alice" {
                            token.as_bytes()
                        } else {
                            id.as_bytes()
                        })
                    ),
                })
                .collect(),
        })
        .unwrap();
        // Exercise the common HTTP boundary with a configured directory, without
        // making this test depend on an external OIDC signing key.
        hub.directory = Arc::new(
            crate::directory::Directory::new(["alice", "bob", "eve"].map(String::from))
                .unwrap()
                .with_tenants(
                    [("alice", "one"), ("bob", "one"), ("eve", "two")]
                        .into_iter()
                        .map(|(id, tenant)| (id.into(), tenant.into()))
                        .collect(),
                )
                .unwrap(),
        );
        let app = app(hub);
        for (method, path, body) in [
            ("GET", "/api/contacts/eve/profile", ""),
            ("GET", "/api/contacts/eve/messages", ""),
            (
                "POST",
                "/api/contacts/eve/messages",
                "{\"request_id\":\"one\",\"body\":\"hi\"}",
            ),
            ("POST", "/api/contacts/eve/read", "{\"through_id\":1}"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {token}"))
                        .header("content-type", "application/json")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/contacts/bob/profile")
                    .header("authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&body).contains("private"));
    }
}
