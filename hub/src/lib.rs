//! Agenct local development control plane. Not a production authentication service.
pub mod browser_auth;
pub mod gitlab;
pub mod oidc;
mod profiles;
pub mod publication;
pub mod worker;
pub mod worker_api;
pub mod worker_consumer;
use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use fs2::FileExt;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone)]
pub enum Runtime {
    Mock,
    Dsh { url: String, token: String },
}
impl Runtime {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Mock => "mock",
            Self::Dsh { .. } => "dsh",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserConfig {
    pub id: String,
    pub token_sha256: String,
}
pub struct Config {
    pub bind: SocketAddr,
    pub database: PathBuf,
    pub users: Vec<UserConfig>,
    pub runtime: Runtime,
}
impl Config {
    pub fn from_env() -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let bind: SocketAddr = std::env::var("AGENCT_BIND")
            .unwrap_or("127.0.0.1:8080".into())
            .parse()?;
        if !bind.ip().is_loopback() {
            return Err("This development service must bind to loopback".into());
        }
        let users = serde_json::from_str(&std::env::var("AGENCT_USERS_JSON")?)?;
        let runtime = match std::env::var("AGENCT_RUNTIME")?.as_str() {
            "mock" => Runtime::Mock,
            "dsh" => {
                let url = std::env::var("AGENCT_ADAPTER_URL")?;
                let parsed = reqwest::Url::parse(&url)?;
                let ip: IpAddr = parsed.host_str().ok_or("adapter host required")?.parse()?;
                if parsed.scheme() != "http"
                    || !ip.is_loopback()
                    || parsed.path() != "/v1/turn"
                    || parsed.query().is_some()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                {
                    return Err("Adapter URL must be a loopback HTTP /v1/turn endpoint".into());
                }
                let token = std::env::var("AGENCT_ADAPTER_TOKEN")?;
                if token.len() < 32 {
                    return Err("Adapter token must contain at least 32 characters".into());
                }
                Runtime::Dsh { url, token }
            }
            _ => return Err("Set AGENCT_RUNTIME explicitly to mock or dsh".into()),
        };
        Ok(Self {
            bind,
            database: std::env::var("AGENCT_DATABASE")
                .unwrap_or("agenct.sqlite3".into())
                .into(),
            users,
            runtime,
        })
    }
}
#[derive(Clone)]
pub struct Hub {
    worker_scopes: Arc<HashMap<String, worker_api::Scope>>,
    browser_auth: Option<browser_auth::BrowserAuth>,
    oidc: Option<oidc::Validator>,
    _lock: Option<Arc<std::fs::File>>,
    db: Arc<Mutex<Connection>>,
    users: Arc<HashMap<String, String>>,
    gates: Arc<HashMap<String, tokio::sync::Mutex<()>>>,
    runtime: Runtime,
    client: reqwest::Client,
}
#[derive(Debug)]
pub struct Error(StatusCode, &'static str);
type Result<T> = std::result::Result<T, Error>;
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error": self.1}))).into_response()
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, "storage error")
    }
}
fn invalid(message: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, message)
}
fn denied() -> Error {
    Error(StatusCode::NOT_FOUND, "not found")
}
impl Hub {
    pub fn open(config: Config) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let mut users = HashMap::new();
        let mut gates = HashMap::new();
        for user in config.users {
            if user.id.is_empty()
                || user.id.len() > 64
                || !user
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                || user.token_sha256.len() != 64
                || !user.token_sha256.chars().all(|c| c.is_ascii_hexdigit())
                || users.contains_key(&user.token_sha256)
                || gates.contains_key(&user.id)
            {
                return Err("invalid or duplicated identity configuration".into());
            }
            gates.insert(user.id.clone(), tokio::sync::Mutex::new(()));
            users.insert(user.token_sha256, user.id);
        }
        if users.is_empty() {
            return Err("at least one configured user is required".into());
        }
        let lock = if config.database.to_str() != Some(":memory:") {
            let mut opts = std::fs::OpenOptions::new();
            opts.read(true).write(true).create(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let guard = opts.open(config.database.with_extension("lock"))?;
            guard
                .try_lock_exclusive()
                .map_err(|_| "database already in use by another Hub")?;
            let _ = opts.open(&config.database)?;
            Some(Arc::new(guard))
        } else {
            None
        };
        let db = Connection::open(config.database)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS sessions (owner TEXT PRIMARY KEY, session_id TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, owner TEXT NOT NULL, role TEXT NOT NULL, content TEXT NOT NULL, status TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL, summary TEXT NOT NULL, status TEXT NOT NULL, result TEXT, revision INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS proposals (id TEXT PRIMARY KEY, requester TEXT NOT NULL, owner TEXT NOT NULL, summary TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', revision INTEGER NOT NULL DEFAULT 0, task_id TEXT UNIQUE, request_id TEXT NOT NULL, source TEXT NOT NULL, UNIQUE(requester,request_id));
CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, task_id TEXT NOT NULL, actor TEXT NOT NULL, action TEXT NOT NULL, at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP);
UPDATE messages SET status='interrupted' WHERE status='pending';")?;
        profiles::initialize(&db)?;
        worker::migrate(&db).map_err(|_| "worker schema migration failed")?;
        let has_revision: bool = db
            .prepare("PRAGMA table_info(tasks)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "revision");
        if !has_revision {
            db.execute(
                "ALTER TABLE tasks ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
                [],
            )?;
        }
        for id in users.values() {
            db.execute(
                "INSERT OR IGNORE INTO sessions(owner,session_id) VALUES (?1,?2)",
                params![id, Uuid::new_v4().to_string()],
            )?;
        }
        Ok(Self {
            worker_scopes: Arc::new(HashMap::new()),
            browser_auth: None,
            oidc: None,
            _lock: lock,
            db: Arc::new(Mutex::new(db)),
            users: Arc::new(users),
            gates: Arc::new(gates),
            runtime: config.runtime,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(75))
                .build()?,
        })
    }
    /// Switches authentication exclusively to pinned external access tokens.
    /// Existing users remain the authorization roster; dev tokens no longer work.
    pub fn with_oidc(mut self, config: oidc::Config) -> std::result::Result<Self, &'static str> {
        if self.browser_auth.is_some() {
            return Err("choose browser sessions or API bearer validation, not both");
        }
        self.oidc = Some(oidc::Validator::new(
            config,
            &self.users.values().cloned().collect(),
        )?);
        Ok(self)
    }
    pub fn with_browser_auth(
        mut self,
        config: browser_auth::Config,
    ) -> std::result::Result<Self, &'static str> {
        if self.oidc.is_some() {
            return Err("choose browser sessions or API bearer validation, not both");
        }
        self.browser_auth = Some(browser_auth::BrowserAuth::new(
            config,
            &self.users.values().cloned().collect(),
        )?);
        Ok(self)
    }
    fn identity(&self, headers: &HeaderMap) -> Result<String> {
        if let Some(auth) = &self.browser_auth {
            return auth.identity(headers);
        }
        if headers.get_all("authorization").iter().count() != 1 {
            return Err(Error(StatusCode::UNAUTHORIZED, "one bearer token required"));
        }
        let token = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or(Error(StatusCode::UNAUTHORIZED, "bearer token required"))?;
        if let Some(validator) = &self.oidc {
            return validator
                .identity(token)
                .map_err(|_| Error(StatusCode::UNAUTHORIZED, "invalid access token"));
        }
        if token.len() < 32 || token.len() > 512 {
            return Err(Error(StatusCode::UNAUTHORIZED, "invalid token"));
        }
        let hash = format!("{:x}", Sha256::digest(token.as_bytes()));
        self.users
            .get(&hash)
            .cloned()
            .ok_or(Error(StatusCode::UNAUTHORIZED, "invalid token"))
    }
    fn db(&self) -> Result<std::sync::MutexGuard<'_, Connection>> {
        self.db
            .lock()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "storage unavailable"))
    }
}
pub fn app(hub: Hub) -> Router {
    Router::new()
        .route("/auth/session", get(browser_auth::session))
        .route("/auth/login", get(browser_auth::login))
        .route("/auth/callback", get(browser_auth::callback))
        .route("/auth/logout", post(browser_auth::logout))
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status":"ok","stage":"development"})) }),
        )
        .route("/api/me", get(me))
        .route(
            "/api/profile",
            get(profiles::get_profile).put(profiles::update_profile),
        )
        .route("/api/profile/catalog", get(profiles::catalog))
        .route("/api/messages", get(messages).post(chat))
        .route("/api/proposals", get(proposals).post(propose))
        .route("/api/proposals/{id}/actions", post(proposal_action))
        .route("/api/tasks", get(tasks).post(delegate))
        .route("/api/tasks/{id}", get(task_detail))
        .route("/api/tasks/{id}/actions", post(action))
        .route(
            "/api/tasks/{id}/worker",
            get(worker_api::status).post(worker_api::enqueue),
        )
        .route(
            "/api/tasks/{id}/worker/{job}/cancel",
            post(worker_api::cancel),
        )
        .fallback_service(tower_http::services::ServeDir::new("hub/web"))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            hub.clone(),
            browser_auth::guard,
        ))
        .with_state(hub)
}
async fn me(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<serde_json::Value>> {
    let id = h.identity(&headers)?;
    Ok(Json(
        serde_json::json!({"user":id,"runtime":h.runtime.label(),"production":false}),
    ))
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
    pub status: String,
}
fn history(h: &Hub, owner: &str) -> Result<Vec<Message>> {
    let db = h.db()?;
    let mut stmt=db.prepare("SELECT role,content,status FROM (SELECT id,role,content,status FROM messages WHERE owner=?1 ORDER BY id DESC LIMIT 40) ORDER BY id")?;
    let rows = stmt.query_map([owner], |r| {
        Ok(Message {
            role: r.get(0)?,
            content: r.get(1)?,
            status: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}
async fn messages(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Message>>> {
    let id = h.identity(&headers)?;
    Ok(Json(history(&h, &id)?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChatInput {
    message: String,
    agent_id: Option<String>,
    request_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterProposal {
    owner: String,
    summary: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdapterReply {
    reply: String,
    backend: String,
    // Explicit null means no proposal; an omitted field is a protocol error.
    #[serde(deserialize_with = "Option::deserialize")]
    proposal: Option<AdapterProposal>,
}
impl AdapterReply {
    fn valid(&self, h: &Hub, requester: &str, message: &str) -> bool {
        self.backend == "dsh"
            && !self.reply.trim().is_empty()
            && self.reply.len() <= 32000
            && self.proposal.as_ref().is_none_or(|p| {
                p.owner != requester
                    && h.gates.contains_key(&p.owner)
                    && message.contains(&p.owner)
                    && !p.summary.trim().is_empty()
                    && p.summary.len() <= 4000
                    && message.contains(&p.summary)
            })
    }
}
async fn chat(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(input): Json<ChatInput>,
) -> Result<Json<serde_json::Value>> {
    let id = h.identity(&headers)?;
    if input.message.trim().is_empty() || input.message.len() > 8000 {
        return Err(invalid("message must contain 1..8000 bytes"));
    }
    let _guard = h
        .gates
        .get(&id)
        .unwrap()
        .try_lock()
        .map_err(|_| Error(StatusCode::CONFLICT, "a turn is already running"))?;
    let runtime_profile = profiles::route(&*h.db()?, &id, input.agent_id.as_deref())?;
    // The deterministic mock is a test double, not a model or DSH tool call.
    // No prior/private history is consulted when constructing the shared summary.
    if matches!(h.runtime, Runtime::Mock) {
        if let Some((owner, summary)) = mock_delegation(&input.message) {
            let proposal = save_proposal(
                &h,
                &id,
                &owner,
                &summary,
                input.request_id.as_deref(),
                &input.message,
            )?;
            return Ok(Json(serde_json::json!({
                "reply":"[MOCK · deterministic test double] 请检查接收人和将共享的任务内容，再明确确认。确认前对方不会收到任务。",
                "backend":"mock", "proposal":proposal, "runtime_profile":runtime_profile
            })));
        }
    }
    let prior: Vec<_> = history(&h, &id)?
        .into_iter()
        .filter(|m| m.status == "complete")
        .collect();
    let (message_id, session_id) = {
        let db = h.db()?;
        db.execute(
            "INSERT INTO messages(owner,role,content,status) VALUES (?1,'user',?2,'pending')",
            params![id, input.message],
        )?;
        let sid: String = db.query_row(
            "SELECT session_id FROM sessions WHERE owner=?1",
            [&id],
            |r| r.get(0),
        )?;
        (db.last_insert_rowid(), sid)
    };
    let answer=match &h.runtime {
        Runtime::Mock=>Ok(AdapterReply {reply: format!("[MOCK · no model] 收到你的消息。当前已保存 {} 条已完成上下文消息。真实任务请使用对话中的委派操作；本回复不会执行工具。",prior.len()), backend:"mock".into(), proposal:None}),
        Runtime::Dsh{url,token}=>{
            let mut payload=serde_json::json!({"session_id":session_id,"runtime_profile":runtime_profile,"history":prior.iter().map(|m|serde_json::json!({"role":m.role,"content":m.content})).collect::<Vec<_>>(),"message":input.message});
            while serde_json::to_vec(&payload).unwrap().len()>60000 {
                payload["history"].as_array_mut().unwrap().remove(0);
            }
            let response=h.client.post(url).bearer_auth(token).json(&payload).send().await;
            match response {
                Ok(r) if r.status().is_success()=>match r.json::<AdapterReply>().await {Ok(r) if r.valid(&h,&id,&input.message)=>Ok(r),_=>Err(())},
                _=>Err(()),
            }
        }
    };
    let mut db = h.db()?;
    let tx = db.transaction()?;
    match answer {
        Ok(answer) => {
            // Model output is untrusted. Only an immutable, sender-private proposal
            // is persisted here; the existing authenticated confirm action creates tasks.
            let proposal = match answer
                .proposal
                .as_ref()
                .map(|p| {
                    save_proposal_tx(
                        &tx,
                        &id,
                        &p.owner,
                        &p.summary,
                        input.request_id.as_deref(),
                        &input.message,
                    )
                })
                .transpose()
            {
                Ok(p) => p,
                Err(error) => {
                    tx.execute(
                        "UPDATE messages SET status='failed' WHERE id=?1",
                        [message_id],
                    )?;
                    tx.commit()?;
                    return Err(error);
                }
            };
            let reply = answer.reply;
            tx.execute(
                "UPDATE messages SET status='complete' WHERE id=?1",
                [message_id],
            )?;
            tx.execute("INSERT INTO messages(owner,role,content,status) VALUES (?1,'assistant',?2,'complete')",params![id,reply])?;
            tx.commit()?;
            Ok(Json(
                serde_json::json!({"reply":reply,"backend":h.runtime.label(),"proposal":proposal,"runtime_profile":runtime_profile}),
            ))
        }
        Err(()) => {
            tx.execute(
                "UPDATE messages SET status='failed' WHERE id=?1",
                [message_id],
            )?;
            tx.commit()?;
            Err(Error(
                StatusCode::BAD_GATEWAY,
                "adapter failed; no mock fallback; task actions were not executed",
            ))
        }
    }
}
#[derive(Serialize)]
struct Task {
    id: String,
    requester: String,
    owner: String,
    summary: String,
    status: String,
    result: Option<String>,
    revision: i64,
}
fn row_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        requester: r.get(1)?,
        owner: r.get(2)?,
        summary: r.get(3)?,
        status: r.get(4)?,
        result: r.get(5)?,
        revision: r.get(6)?,
    })
}
async fn tasks(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Task>>> {
    let id = h.identity(&headers)?;
    let db = h.db()?;
    let mut q=db.prepare("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE requester=?1 OR owner=?1 ORDER BY rowid DESC")?;
    let rows = q.query_map([id], row_task)?;
    Ok(Json(rows.collect::<std::result::Result<Vec<_>, _>>()?))
}
async fn task_detail(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(task): Path<String>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let db = h.db()?;
    let t=db.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",params![task,user],row_task).map_err(|_|denied())?;
    let mut q = db.prepare("SELECT actor,action,at FROM events WHERE task_id=?1 ORDER BY id")?;
    let events=q.query_map([task],|r|Ok(serde_json::json!({"actor":r.get::<_,String>(0)?,"action":r.get::<_,String>(1)?,"at":r.get::<_,String>(2)?})))?.collect::<std::result::Result<Vec<_>,_>>()?;
    Ok(Json(serde_json::json!({"task":t,"events":events})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Delegate {
    owner: String,
    summary: String,
}
async fn delegate(
    State(h): State<Hub>,
    headers: HeaderMap,
    Json(d): Json<Delegate>,
) -> Result<Json<serde_json::Value>> {
    let requester = h.identity(&headers)?;
    if d.owner == requester || !h.gates.contains_key(&d.owner) {
        return Err(invalid("select another configured participant"));
    }
    if d.summary.trim().is_empty() || d.summary.len() > 4000 {
        return Err(invalid("summary must contain 1..4000 bytes"));
    }
    let id = Uuid::new_v4().to_string();
    let mut db = h.db()?;
    let tx = db.transaction()?;
    tx.execute("INSERT INTO tasks(id,requester,owner,summary,status) VALUES (?1,?2,?3,?4,'awaiting_owner')",params![id,requester,d.owner,d.summary])?;
    tx.execute(
        "INSERT INTO events(task_id,actor,action) VALUES (?1,?2,'created')",
        params![id, requester],
    )?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"id":id,"status":"awaiting_owner","revision":0}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Action {
    action: String,
    expected_revision: i64,
    result: Option<String>,
}
async fn action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<Action>,
) -> Result<Json<serde_json::Value>> {
    let user = h.identity(&headers)?;
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let t=tx.query_row("SELECT id,requester,owner,summary,status,result,revision FROM tasks WHERE id=?1 AND (requester=?2 OR owner=?2)",params![id,user],row_task).map_err(|_|denied())?;
    if a.expected_revision != t.revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "task changed; refresh and review before acting",
        ));
    }
    let next = match (a.action.as_str(), t.status.as_str()) {
        ("accept", "awaiting_owner") if user == t.owner => "working",
        ("deliver", "working") if user == t.owner => {
            if a.result
                .as_ref()
                .is_none_or(|v| v.trim().is_empty() || v.len() > 8000)
            {
                return Err(invalid("delivery requires a 1..8000 byte result"));
            }
            "ready_for_acceptance"
        }
        ("approve", "ready_for_acceptance") if user == t.requester => "completed",
        ("return", "ready_for_acceptance") if user == t.requester => "awaiting_owner",
        ("cancel", s) if s != "completed" && s != "canceled" => "canceled",
        _ => {
            return Err(Error(
                StatusCode::CONFLICT,
                "action not allowed for identity or current state",
            ))
        }
    };
    let result = if a.action == "deliver" {
        a.result
    } else if a.action == "return" {
        None
    } else {
        t.result
    };
    tx.execute(
        "UPDATE tasks SET status=?1,result=?2,revision=revision+1 WHERE id=?3",
        params![next, result, id],
    )?;
    tx.execute(
        "INSERT INTO events(task_id,actor,action) VALUES (?1,?2,?3)",
        params![id, user, a.action],
    )?;
    // Task revision changes revoke queued work and lease capabilities atomically.
    worker::revoke_task(&tx, &id)?;
    tx.commit()?;
    Ok(Json(
        serde_json::json!({"id":id,"status":next,"revision":t.revision+1}),
    ))
}

// Intentionally bounded recognition for the offline acceptance fixture. DSH replies
// remain plain text and have no task-creation authority.
fn mock_delegation(message: &str) -> Option<(String, String)> {
    let text = message.trim();
    let rest = text
        .strip_prefix("请让 ")
        .or_else(|| text.strip_prefix("ask "))?;
    let (owner, rest) = rest.split_once(' ')?;
    let summary = rest
        .strip_prefix("帮我 ")
        .or_else(|| rest.strip_prefix("to "))?;
    Some((owner.to_owned(), summary.to_owned()))
}
#[derive(Serialize)]
struct Proposal {
    id: String,
    requester: String,
    owner: String,
    summary: String,
    status: String,
    revision: i64,
    task_id: Option<String>,
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
    })
}
const PROPOSAL_COLUMNS: &str = "id,requester,owner,summary,status,revision,task_id";
fn save_proposal(
    h: &Hub,
    requester: &str,
    owner: &str,
    summary: &str,
    request_id: Option<&str>,
    source: &str,
) -> Result<Proposal> {
    if owner == requester || !h.gates.contains_key(owner) {
        return Err(invalid("select another configured participant"));
    }
    if summary.trim().is_empty() || summary.len() > 4000 {
        return Err(invalid("summary must contain 1..4000 bytes"));
    }
    let mut db = h.db()?;
    let tx = db.transaction()?;
    let proposal = save_proposal_tx(&tx, requester, owner, summary, request_id, source)?;
    tx.commit()?;
    Ok(proposal)
}
fn save_proposal_tx(
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
    let existing = tx.query_row(&format!("SELECT {PROPOSAL_COLUMNS},source FROM proposals WHERE requester=?1 AND request_id=?2"),params![requester,key],|r|Ok((row_proposal(r)?,r.get::<_,String>(7)?))).optional()?;
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
    tx.execute("INSERT INTO proposals(id,requester,owner,summary,request_id,source) VALUES (?1,?2,?3,?4,?5,?6)",params![id,requester,owner,summary,key,source])?;
    let p = tx.query_row(
        &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1"),
        [id],
        row_proposal,
    )?;
    Ok(p)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposalInput {
    owner: String,
    summary: String,
    request_id: Option<String>,
}
async fn propose(
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
async fn proposals(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<Proposal>>> {
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
struct ProposalAction {
    action: String,
    expected_revision: i64,
}
async fn proposal_action(
    State(h): State<Hub>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(a): Json<ProposalAction>,
) -> Result<Json<Proposal>> {
    let user = h.identity(&headers)?;
    if a.action != "confirm" && a.action != "reject" {
        return Err(invalid("unsupported proposal action"));
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
    // Retry of the identical action at the reviewed revision returns the same result.
    // This cannot authorize different content: proposal fields are immutable.
    let target = if a.action == "confirm" {
        "confirmed"
    } else {
        "rejected"
    };
    if p.status == target && a.expected_revision == 0 && p.revision == 1 {
        return Ok(Json(p));
    }
    if p.status != "pending" || p.revision != a.expected_revision {
        return Err(Error(
            StatusCode::CONFLICT,
            "proposal changed; refresh and review",
        ));
    }
    if a.action == "confirm" && (!h.gates.contains_key(&p.owner) || p.owner == user) {
        return Err(invalid("recipient is no longer available"));
    }
    let task_id = if a.action == "confirm" {
        let task = Uuid::new_v4().to_string();
        tx.execute("INSERT INTO tasks(id,requester,owner,summary,status) VALUES (?1,?2,?3,?4,'awaiting_owner')",params![task,user,p.owner,p.summary])?;
        tx.execute(
            "INSERT INTO events(task_id,actor,action) VALUES (?1,?2,'created')",
            params![task, user],
        )?;
        Some(task)
    } else {
        None
    };
    tx.execute(
        "UPDATE proposals SET status=?1,revision=revision+1,task_id=?2 WHERE id=?3",
        params![target, task_id, id],
    )?;
    let result = tx.query_row(
        &format!("SELECT {PROPOSAL_COLUMNS} FROM proposals WHERE id=?1"),
        [id],
        row_proposal,
    )?;
    tx.commit()?;
    Ok(Json(result))
}
