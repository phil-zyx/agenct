//! Agenct local development control plane. Not a production authentication service.
mod activity;
mod audit;
mod audit_migration;
mod auto_drafts;
pub mod browser_auth;
mod chat_failures;
mod chat_turns;
mod collaboration;
mod colleague_lookup;
mod context_memory;
mod directory;
mod drafts;
pub mod gitlab;
mod materials;
mod members;
mod messages;
pub mod oidc;
mod policy;
mod private_staging;
mod profile_suggestions;
mod profiles;
mod project_context;
mod project_questions;
mod project_state;
mod projects;
mod proposals;
pub mod publication;
mod repo_context;
mod runtime;
mod schema;
mod task_context;
mod tasks;
pub mod tenants;
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
pub use messages::Message;
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
            let explicitly_allowed =
                std::env::var("AGENCT_PRIVATE_BIND_IP").ok() == Some(bind.ip().to_string());
            if !explicitly_allowed
                || !browser_auth::private_http(&format!("http://{bind}"))
                || std::env::var_os("AGENCT_BROWSER_OIDC_CONFIG_FILE").is_none()
            {
                return Err("Non-loopback development bind requires an explicit private IP and browser authentication".into());
            }
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
/// Token SHA-256 → user ID.
type Roster = Arc<HashMap<String, String>>;
#[derive(Clone)]
pub struct Hub {
    member_admins: Arc<std::collections::HashSet<String>>,
    repositories: Option<Arc<repo_context::Repositories>>,
    worker_scopes: Arc<HashMap<String, worker_api::Scope>>,
    browser_auth: Option<browser_auth::BrowserAuth>,
    oidc: Option<oidc::Validator>,
    _lock: Option<Arc<std::fs::File>>,
    db: Arc<Mutex<Connection>>,
    users: Arc<HashMap<String, String>>,
    private_config: Option<PathBuf>,
    /// Last accepted private roster, keyed by the config file's SHA-256, so an
    /// unchanged file is not re-parsed and re-applied on every request.
    private_users: Arc<Mutex<Option<(String, Roster)>>>,
    /// Startup tenant audit, fixed once the router is built (`app`). `None` means
    /// a direct library caller: audit on every identity check, fail closed.
    startup_ok: Option<bool>,
    /// Summaries of large, append-only shared task material, so a turn that
    /// references the same material does not re-summarize it every time.
    material_summaries: Arc<Mutex<HashMap<String, String>>>,
    /// Start the owner's private draft when a task enters `working`.
    auto_draft: bool,
    auto_draft_slots: Arc<tokio::sync::Semaphore>,
    auto_draft_notify: Arc<tokio::sync::Notify>,
    extra_gates: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    directory: Arc<directory::Directory>,
    // Arc per user so a turn can own its gate across a detached dispatch task.
    gates: Arc<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    runtime: Runtime,
    client: reqwest::Client,
}
#[derive(Debug)]
pub struct Error(StatusCode, &'static str);
type Result<T> = std::result::Result<T, Error>;
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        // Selection codes describe rejection before reservation. Explicit adapter
        // failure codes describe an ended dispatch, never proof it did no work.
        // Generic conflicts and transport failures remain unknown to clients.
        let code = match self.1 {
            drafts::STALE => Some("draft_task_stale_before_reservation"),
            drafts::STATE => Some("draft_task_state_before_reservation"),
            drafts::ROLE => Some("draft_role_unavailable_before_reservation"),
            drafts::BUSY => Some("draft_busy_before_reservation"),
            profiles::STALE => Some("selected_agent_stale_before_dispatch"),
            profiles::UNAVAILABLE => Some("selected_agent_unavailable_before_dispatch"),
            task_context::STALE => Some("selected_task_stale_before_dispatch"),
            task_context::UNAVAILABLE => Some("selected_task_unavailable_before_dispatch"),
            _ => chat_failures::code(&self),
        };
        let mut body = serde_json::json!({"error": self.1});
        if let Some(code) = code {
            body["code"] = serde_json::json!(code);
        }
        (self.0, Json(body)).into_response()
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
        let directory = directory::Directory::new(config.users.iter().map(|user| user.id.clone()))?;
        let gates = directory
            .ids()
            .map(|id| (id.clone(), Arc::new(tokio::sync::Mutex::new(()))))
            .collect();
        let mut users = HashMap::new();
        for user in config.users {
            if user.token_sha256.len() != 64
                || !user.token_sha256.chars().all(|c| c.is_ascii_hexdigit())
                || users.contains_key(&user.token_sha256)
            {
                return Err("invalid or duplicated identity configuration".into());
            }
            users.insert(user.token_sha256, user.id);
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
        let mut db = Connection::open(config.database)?;
        // Reject newer databases before WAL mode, schema changes or recovery writes.
        schema::migrate(&mut db)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        schema::recover(&mut db)?;
        for id in users.values() {
            db.execute(
                "INSERT OR IGNORE INTO sessions(owner,session_id) VALUES (?1,?2)",
                params![id, Uuid::new_v4().to_string()],
            )?;
        }
        Ok(Self {
            member_admins: Arc::new(Default::default()),
            repositories: None,
            worker_scopes: Arc::new(HashMap::new()),
            browser_auth: None,
            oidc: None,
            _lock: lock,
            db: Arc::new(Mutex::new(db)),
            users: Arc::new(users),
            private_config: None,
            private_users: Arc::new(Mutex::new(None)),
            startup_ok: None,
            material_summaries: Arc::new(Mutex::new(HashMap::new())),
            auto_draft: false,
            auto_draft_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            auto_draft_notify: Arc::new(tokio::sync::Notify::new()),
            extra_gates: Arc::new(Mutex::new(HashMap::new())),
            directory: Arc::new(directory),
            gates: Arc::new(gates),
            runtime: config.runtime,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(75))
                .build()?,
        })
    }
    /// When a task is accepted or returned to `working`, draft privately for its
    /// owner in the background. Drafts are never shared or delivered automatically.
    pub fn with_auto_draft(mut self, enabled: bool) -> Self {
        self.auto_draft = enabled;
        self
    }
    /// Switches authentication exclusively to pinned external access tokens.
    /// Existing users remain the authorization roster; dev tokens no longer work.
    pub fn with_oidc(mut self, config: oidc::Config) -> std::result::Result<Self, &'static str> {
        if self.private_config.is_some() {
            return Err("OIDC cannot be combined with live private membership");
        }
        if self.browser_auth.is_some() {
            return Err("choose browser sessions or API bearer validation, not both");
        }
        self.oidc = Some(oidc::Validator::new(
            config,
            &self.directory.ids().collect(),
        )?);
        Ok(self)
    }
    pub fn with_browser_auth(
        mut self,
        config: browser_auth::Config,
    ) -> std::result::Result<Self, &'static str> {
        if self.private_config.is_some() {
            return Err("browser sessions cannot be combined with live private membership");
        }
        if self.oidc.is_some() {
            return Err("choose browser sessions or API bearer validation, not both");
        }
        let auto_signup = config.gitlab_auto_signup;
        let issuer = config.issuer.clone();
        let subjects = config.subjects.clone();
        self.browser_auth = Some(browser_auth::BrowserAuth::new(
            config,
            &self.directory.ids().collect(),
        )?);
        if auto_signup {
            self.restore_gitlab_workspace(&issuer)?;
        } else {
            self.bind_subjects(&issuer, &subjects)?;
        }
        Ok(self)
    }
    fn identity(&self, headers: &HeaderMap) -> Result<String> {
        let id = self.raw_identity(headers)?;
        if !self.member_active(&id)? {
            return Err(Error(
                StatusCode::UNAUTHORIZED,
                "account disabled or invitation pending",
            ));
        }
        Ok(id)
    }
    fn raw_identity(&self, headers: &HeaderMap) -> Result<String> {
        let startup = match self.startup_ok {
            Some(ok) => ok,
            None => self.validate_startup().is_ok(),
        };
        startup.then_some(()).ok_or(()).map_err(|_| {
            Error(
                StatusCode::SERVICE_UNAVAILABLE,
                "tenant configuration required for this database",
            )
        })?;
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
        let live_users = self.reload_private_users()?;
        live_users
            .as_deref()
            .unwrap_or(&self.users)
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
pub fn app(mut hub: Hub) -> Router {
    // Tenant membership is written only by builders, so the audit result cannot
    // change once routing starts; record it instead of querying per request.
    hub.startup_ok = Some(hub.validate_startup().is_ok());
    Router::new()
        .route("/auth/session", get(browser_auth::session))
        .route("/auth/login", get(browser_auth::login))
        .route("/auth/callback", get(browser_auth::callback))
        .route(
            "/identity/v1/auth/oidc/gitlab/callback",
            get(browser_auth::callback),
        )
        .route("/auth/logout", post(browser_auth::logout))
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status":"ok","stage":"development"})) }),
        )
        .route("/api/me", get(me))
        .route("/api/contacts", get(contacts))
        .route(
            "/api/collaboration-profile",
            get(collaboration::get_profile)
                .put(collaboration::update_profile)
                .layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/api/contacts/{id}/profile",
            get(collaboration::public_profile),
        )
        .route(
            "/api/contacts/{id}/messages",
            get(collaboration::messages)
                .post(collaboration::send)
                .layer(DefaultBodyLimit::max(32 * 1024)),
        )
        .route("/api/contacts/{id}/read", post(collaboration::mark_read))
        .route("/api/conversations", get(collaboration::conversations))
        .route("/api/projects", get(projects::list).post(projects::create))
        .route("/api/project-invitations", get(projects::invitations))
        .route("/api/projects/{id}", get(projects::detail))
        .route(
            "/api/projects/{id}/state",
            get(project_state::list).post(project_state::create),
        )
        .route(
            "/api/projects/{id}/state/{item}",
            get(project_state::history),
        )
        .route(
            "/api/projects/{id}/state/{item}/actions",
            post(project_state::action),
        )
        .route(
            "/api/projects/{id}/state/{item}/questions",
            get(project_questions::list).post(project_questions::create),
        )
        .route(
            "/api/projects/{id}/state/{item}/questions/{message}/withdraw",
            post(project_questions::withdraw),
        )
        .route("/api/projects/{id}/members", post(projects::invite))
        .route(
            "/api/projects/{id}/membership",
            get(projects::membership_status).post(projects::change_membership),
        )
        .route(
            "/api/projects/{id}/members/{member}/remove",
            post(projects::remove_member),
        )
        .route("/api/projects/{id}/archive", post(projects::archive))
        .route("/api/projects/{id}/tasks", post(projects::link_task))
        .route(
            "/api/projects/{id}/tasks/{task}/actions",
            post(projects::link_action),
        )
        .route("/api/profile-suggestions", get(profile_suggestions::list))
        .route(
            "/api/profile-suggestions/{id}/actions",
            post(profile_suggestions::action),
        )
        .route("/api/people", get(members::people))
        .route("/api/members", get(members::list))
        .route("/api/members/{id}", axum::routing::put(members::update))
        .route("/api/invitations", post(members::invite))
        .route("/api/invitations/accept", post(members::accept))
        .route("/api/repositories", get(repo_context::list))
        .route("/api/repositories/context", post(repo_context::read))
        .route(
            "/api/materials/extract",
            post(materials::extract).layer(DefaultBodyLimit::max(3 * 1024 * 1024)),
        )
        .route(
            "/api/profile",
            get(profiles::get_profile).put(profiles::update_profile),
        )
        .route("/api/profile/catalog", get(profiles::catalog))
        .route(
            "/api/messages",
            get(messages::messages).post(chat_turns::chat),
        )
        .route("/api/turns/{id}", get(chat_turns::status))
        .route("/api/activity", get(activity::activity))
        .route(
            "/api/proposals",
            get(proposals::proposals).post(proposals::propose),
        )
        .route(
            "/api/proposals/{id}/actions",
            post(proposals::proposal_action).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route("/api/tasks", get(tasks::tasks))
        .route("/api/tasks/{id}", get(tasks::task_detail))
        .route(
            "/api/tasks/{id}/drafts",
            get(drafts::list).post(drafts::create),
        )
        .route(
            "/api/tasks/{id}/drafts/turns/{request_id}",
            get(drafts::status),
        )
        .route("/api/tasks/{id}/actions", post(tasks::action))
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
        .layer(axum::middleware::from_fn(private_staging::event_log))
        .layer(axum::middleware::from_fn_with_state(
            hub.clone(),
            browser_auth::guard,
        ))
        .with_state(hub)
}
async fn me(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<serde_json::Value>> {
    let id = h.identity(&headers)?;
    let mut value = serde_json::json!({"user":id,"runtime":h.runtime.label(),"production":false});
    if let Some(tenant) = h.directory.tenant(&id) {
        value["tenant"] = serde_json::json!(tenant);
    }
    value["member"] = serde_json::to_value(h.member(&id)?).map_err(|_| {
        Error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "member serialization failed",
        )
    })?;
    Ok(Json(value))
}

async fn contacts(State(h): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<String>>> {
    let id = h.identity(&headers)?;
    let mut ids = Vec::new();
    for other in h.directory.contacts(&id) {
        if h.member_active(&other)? {
            ids.push(other);
        }
    }
    Ok(Json(ids))
}
