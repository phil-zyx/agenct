//! Agenct local development control plane. Not a production authentication service.
mod activity;
mod audit;
mod audit_migration;
pub mod browser_auth;
mod chat_turns;
mod directory;
mod drafts;
pub mod gitlab;
mod messages;
pub mod oidc;
mod policy;
mod profiles;
mod proposals;
pub mod publication;
mod runtime;
mod schema;
mod task_context;
mod tasks;
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
        // Only these selection checks run before receipt reservation/dispatch.
        // Generic conflicts and transport failures must remain uncertain to clients.
        let code = match self.1 {
            drafts::STALE => Some("draft_task_stale_before_reservation"),
            drafts::STATE => Some("draft_task_state_before_reservation"),
            drafts::ROLE => Some("draft_role_unavailable_before_reservation"),
            drafts::BUSY => Some("draft_busy_before_reservation"),
            profiles::STALE => Some("selected_agent_stale_before_dispatch"),
            profiles::UNAVAILABLE => Some("selected_agent_unavailable_before_dispatch"),
            task_context::STALE => Some("selected_task_stale_before_dispatch"),
            task_context::UNAVAILABLE => Some("selected_task_unavailable_before_dispatch"),
            _ => None,
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
            worker_scopes: Arc::new(HashMap::new()),
            browser_auth: None,
            oidc: None,
            _lock: lock,
            db: Arc::new(Mutex::new(db)),
            users: Arc::new(users),
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
    /// Switches authentication exclusively to pinned external access tokens.
    /// Existing users remain the authorization roster; dev tokens no longer work.
    pub fn with_oidc(mut self, config: oidc::Config) -> std::result::Result<Self, &'static str> {
        if self.browser_auth.is_some() {
            return Err("choose browser sessions or API bearer validation, not both");
        }
        self.oidc = Some(oidc::Validator::new(
            config,
            &self.directory.ids().cloned().collect(),
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
            &self.directory.ids().cloned().collect(),
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
            post(proposals::proposal_action),
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
