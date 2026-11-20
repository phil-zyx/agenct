//! Agenct v2 — Minimalist single-stack personal super-agent Hub.
pub mod attachments;
pub mod auth;
pub mod automation;
pub mod brain;
pub mod collab;
pub mod config;
pub mod db;
pub mod events;
pub mod integrations;
pub mod llm;
pub mod nodes;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Json, Router,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Debug)]
pub struct Error(pub StatusCode, pub &'static str);
pub type Result<T> = std::result::Result<T, Error>;

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

pub fn invalid(msg: &'static str) -> Error {
    Error(StatusCode::BAD_REQUEST, msg)
}

pub fn denied() -> Error {
    Error(StatusCode::NOT_FOUND, "not found")
}

#[derive(Clone)]
pub struct Hub {
    pub config: Arc<config::Config>,
    pub db: db::Db,
    pub llm: llm::LlmClient,
    pub events: events::EventBus,
    pub http: reqwest::Client,
    pub oauth_pending: auth::OAuthPendingStore,
    pub attachments_dir: PathBuf,
    pub scheduler_notify: Arc<tokio::sync::Notify>,
    pub scheduler: Arc<automation::SchedulerState>,
    gates: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    compacting: Arc<Mutex<HashSet<String>>>,
}

impl Hub {
    pub fn open(cfg: config::Config) -> std::result::Result<Self, Box<dyn std::error::Error>> {
        let db = db::Db::open(&cfg.database_path)?;
        let attachments_dir = if cfg.is_memory_db() {
            std::env::temp_dir().join(format!("agenct-att-{}", uuid::Uuid::new_v4().simple()))
        } else {
            cfg.data_dir.join("attachments")
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .user_agent("agenct-hub/2.0")
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()?;
        let llm = llm::LlmClient::new(cfg.llm.clone(), http.clone());

        Ok(Self {
            config: Arc::new(cfg),
            db,
            llm,
            events: events::EventBus::default(),
            http,
            oauth_pending: auth::OAuthPendingStore::default(),
            attachments_dir,
            scheduler_notify: Arc::new(tokio::sync::Notify::new()),
            scheduler: Arc::new(automation::SchedulerState::default()),
            gates: Arc::new(Mutex::new(HashMap::new())),
            compacting: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub fn user_gate(&self, user_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.gates.lock().expect("gates mutex");
        map.entry(user_id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub fn try_begin_compaction(&self, user_id: &str) -> bool {
        let mut set = self.compacting.lock().expect("compacting mutex");
        set.insert(user_id.to_owned())
    }

    pub fn end_compaction(&self, user_id: &str) {
        let mut set = self.compacting.lock().expect("compacting mutex");
        set.remove(user_id);
    }
}

pub fn app(hub: Hub) -> Router {
    let static_dir = std::env::var("AGENCT_WEB_DIR").unwrap_or_else(|_| "hub/web".into());
    Router::new()
        // Health
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status": "ok", "version": "2.0"})) }),
        )
        // Auth (GitLab OAuth only)
        .route("/auth/session", get(auth::session))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route(
            "/identity/v1/auth/oidc/gitlab/callback",
            get(auth::callback),
        )
        .route("/auth/logout", post(auth::logout))
        // Core Brain & Realtime Stream
        .route("/api/bootstrap", get(brain::bootstrap))
        .route("/api/events", get(events::stream))
        .route("/api/chat", post(brain::chat))
        .route("/api/brain", put(brain::save_profile))
        .route("/api/ledgers", post(brain::upsert_ledger))
        .route("/api/ledgers/{id}", delete(brain::delete_ledger))
        .route(
            "/api/threads/{id}",
            put(brain::update_thread).delete(brain::delete_thread),
        )
        // Tasks & Lightweight Collaboration
        .route("/api/tasks/{id}/action", post(collab::task_action))
        .route("/api/members", get(auth::list_members))
        .route("/api/members/{id}/status", put(auth::set_member_status))
        // Attachments
        .route(
            "/api/attachments",
            post(attachments::upload).layer(DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/api/attachments/{id}", get(attachments::download))
        // Unified Execution Nodes (PC & Cloud Runners)
        .route("/api/nodes", get(nodes::list).post(nodes::create))
        .route("/api/nodes/{id}/token", post(nodes::rotate_token))
        .route("/api/nodes/{id}", delete(nodes::remove))
        .route("/node/agenct-node.py", get(nodes::agent_script))
        .route("/node/install.sh", get(nodes::install_sh))
        .route("/node/install.ps1", get(nodes::install_ps1))
        .route("/node/v1/work", get(nodes::work))
        .route("/node/v1/tasks/{id}/lease", post(nodes::renew_lease))
        .route(
            "/node/v1/tasks/{id}/submit",
            post(nodes::submit_result).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        // Automation (Permissions, Rules, Crons)
        .route("/api/automation", get(automation::get_automation))
        .route(
            "/api/automation/permissions",
            put(automation::update_permissions),
        )
        .route("/api/automation/rules", post(automation::create_rule))
        .route(
            "/api/automation/rules/{id}",
            delete(automation::delete_rule),
        )
        .route("/api/automation/crons", post(automation::create_cron))
        .route(
            "/api/automation/crons/{id}",
            delete(automation::delete_cron),
        )
        // Integrations (GitLab & IM)
        .route(
            "/api/integrations/gitlab",
            get(integrations::gitlab::get_config).put(integrations::gitlab::save_config),
        )
        .route(
            "/api/integrations/im",
            get(integrations::im::get_config).put(integrations::im::save_config),
        )
        .route(
            "/api/integrations/im/test",
            post(integrations::im::test_webhook),
        )
        .route("/webhooks/gitlab", post(integrations::gitlab::webhook))
        .route("/webhooks/im", post(integrations::im::inbound_webhook))
        // Static Web UI
        .fallback_service(tower_http::services::ServeDir::new(static_dir))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(axum::middleware::from_fn_with_state(
            hub.clone(),
            auth::guard,
        ))
        .with_state(hub)
}
