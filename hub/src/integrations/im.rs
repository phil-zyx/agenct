//! Outbound WebSocket IM integrations (WeCom long-connection + DingTalk Stream + signed webhooks).
use crate::{
    auth::{random_token, sha256_hex, AuthUser},
    brain::run_turn,
    config::{
        constant_time_eq, now_unix, truncate_bytes, validate_im_webhook_url, validate_outbound_host,
    },
    db::record_audit,
    invalid,
    secrets::SCOPE_IM,
    Error, Hub, Result,
};
use axum::{extract::State, http::StatusCode, Json};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{SinkExt, StreamExt};
use ring::hmac;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::mpsc,
    task::AbortHandle,
    time::{Instant, MissedTickBehavior},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use uuid::Uuid;

const WECOM_DEFAULT_WS_URL: &str = "wss://openws.work.weixin.qq.com";
const DINGTALK_DEFAULT_API_BASE: &str = "https://api.dingtalk.com";
const BIND_CODE_TTL_SECS: i64 = 900;
const BIND_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

fn default_provider() -> String {
    "wecom".into()
}
fn default_true() -> bool {
    true
}
fn default_chat_type_single() -> u8 {
    1
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WeComBotConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub bot_id: String,
    #[serde(default)]
    pub secret: String,
    /// Test/private override; ignored unless `AGENCT_ALLOW_PRIVATE_OUTBOUND=1`.
    #[serde(default)]
    pub ws_url: String,
    #[serde(default)]
    pub bound_user_id: String,
    #[serde(default)]
    pub push_chat_id: String,
    #[serde(default = "default_chat_type_single")]
    pub push_chat_type: u8,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DingTalkBotConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    /// Test/private override; ignored unless `AGENCT_ALLOW_PRIVATE_OUTBOUND=1`.
    #[serde(default)]
    pub api_base: String,
    #[serde(default)]
    pub bound_staff_id: String,
    #[serde(default)]
    pub robot_code: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImConfig {
    #[serde(default)]
    pub wecom: WeComBotConfig,
    #[serde(default)]
    pub dingtalk: DingTalkBotConfig,
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub webhook_secret: String,
    #[serde(default)]
    pub bind_code_hash: String,
    #[serde(default)]
    pub bind_code_expires_at: i64,
    #[serde(default = "default_true")]
    pub notify_delegation: bool,
    #[serde(default = "default_true")]
    pub notify_delivery: bool,
}

impl Default for ImConfig {
    fn default() -> Self {
        Self {
            wecom: WeComBotConfig::default(),
            dingtalk: DingTalkBotConfig::default(),
            provider: default_provider(),
            webhook_url: String::new(),
            webhook_secret: String::new(),
            bind_code_hash: String::new(),
            bind_code_expires_at: 0,
            notify_delegation: true,
            notify_delivery: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ChannelStatus {
    pub state: String,
    pub last_error: String,
    pub updated_at: i64,
}

impl Default for ChannelStatus {
    fn default() -> Self {
        Self {
            state: "disabled".into(),
            last_error: String::new(),
            updated_at: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct CachedToken {
    token: String,
    expires_at: i64,
}

#[derive(Default)]
struct UserRuntimeState {
    wecom_handle: Option<AbortHandle>,
    wecom_generation: u64,
    wecom_tx: Option<mpsc::Sender<Value>>,
    wecom_status: ChannelStatus,
    dingtalk_handle: Option<AbortHandle>,
    dingtalk_generation: u64,
    dingtalk_status: ChannelStatus,
    dd_session_webhook: Option<(String, i64)>,
    dd_token: Option<CachedToken>,
}

#[derive(Default)]
struct RuntimeInner {
    users: HashMap<String, UserRuntimeState>,
}

impl Drop for RuntimeInner {
    fn drop(&mut self) {
        for user in self.users.values_mut() {
            if let Some(h) = user.wecom_handle.take() {
                h.abort();
            }
            if let Some(h) = user.dingtalk_handle.take() {
                h.abort();
            }
        }
    }
}

/// Process-wide manager for per-user WeCom/DingTalk outbound WebSocket workers.
#[derive(Clone, Default)]
pub struct ImRuntime {
    inner: Arc<Mutex<RuntimeInner>>,
}

impl ImRuntime {
    pub fn shutdown_all(&self) {
        let mut inner = self.inner.lock().expect("im_runtime mutex");
        for user in inner.users.values_mut() {
            if let Some(h) = user.wecom_handle.take() {
                h.abort();
            }
            if let Some(h) = user.dingtalk_handle.take() {
                h.abort();
            }
            user.wecom_tx = None;
        }
    }

    pub async fn start_all(&self, hub: &Hub) {
        let uids: Vec<String> = hub
            .db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT i.user_id FROM integrations i
                     JOIN users u ON u.id = i.user_id
                     WHERE i.kind = 'im' AND u.status = 'active'",
                )?;
                let rows = stmt
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .unwrap_or_default();
        for uid in uids {
            self.sync_user(hub, &uid).await;
        }
    }

    pub async fn sync_user(&self, hub: &Hub, user_id: &str) {
        let Ok(cfg) = load_im_config(hub, user_id).await else {
            return;
        };
        let active_user: bool = hub
            .db
            .read({
                let uid = user_id.to_owned();
                move |conn| {
                    Ok(conn
                        .query_row("SELECT status FROM users WHERE id = ?1", [&uid], |r| {
                            r.get::<_, String>(0)
                        })
                        .optional()?
                        .is_some_and(|s| s == "active"))
                }
            })
            .await
            .unwrap_or(false);

        let mut start_wecom = None;
        let mut start_dingtalk = None;
        {
            let mut inner = self.inner.lock().expect("im_runtime mutex");
            let entry = inner.users.entry(user_id.to_owned()).or_default();

            if let Some(h) = entry.wecom_handle.take() {
                h.abort();
            }
            entry.wecom_tx = None;
            if active_user
                && cfg.wecom.enabled
                && !cfg.wecom.bot_id.is_empty()
                && !cfg.wecom.secret.is_empty()
            {
                entry.wecom_generation = entry.wecom_generation.wrapping_add(1);
                entry.wecom_status = ChannelStatus {
                    state: "connecting".into(),
                    last_error: String::new(),
                    updated_at: now_unix(),
                };
                start_wecom = Some((entry.wecom_generation, cfg.wecom.clone()));
            } else {
                entry.wecom_status = ChannelStatus::default();
            }

            if let Some(h) = entry.dingtalk_handle.take() {
                h.abort();
            }
            if active_user
                && cfg.dingtalk.enabled
                && !cfg.dingtalk.client_id.is_empty()
                && !cfg.dingtalk.client_secret.is_empty()
            {
                entry.dingtalk_generation = entry.dingtalk_generation.wrapping_add(1);
                entry.dingtalk_status = ChannelStatus {
                    state: "connecting".into(),
                    last_error: String::new(),
                    updated_at: now_unix(),
                };
                start_dingtalk = Some((entry.dingtalk_generation, cfg.dingtalk.clone()));
            } else {
                entry.dingtalk_status = ChannelStatus::default();
            }
        }

        if let Some((gen_id, wcfg)) = start_wecom {
            let hub_c = hub.clone();
            let uid = user_id.to_owned();
            let handle = tokio::spawn(async move {
                run_wecom_supervisor(hub_c, uid, gen_id, wcfg).await;
            });
            let mut inner = self.inner.lock().expect("im_runtime mutex");
            if let Some(entry) = inner.users.get_mut(user_id) {
                if entry.wecom_generation == gen_id {
                    entry.wecom_handle = Some(handle.abort_handle());
                } else {
                    handle.abort();
                }
            } else {
                handle.abort();
            }
        }

        if let Some((gen_id, dcfg)) = start_dingtalk {
            let hub_c = hub.clone();
            let uid = user_id.to_owned();
            let handle = tokio::spawn(async move {
                run_dingtalk_supervisor(hub_c, uid, gen_id, dcfg).await;
            });
            let mut inner = self.inner.lock().expect("im_runtime mutex");
            if let Some(entry) = inner.users.get_mut(user_id) {
                if entry.dingtalk_generation == gen_id {
                    entry.dingtalk_handle = Some(handle.abort_handle());
                } else {
                    handle.abort();
                }
            } else {
                handle.abort();
            }
        }
    }

    fn set_wecom_state(
        &self,
        hub: &Hub,
        user_id: &str,
        gen_id: u64,
        state: &str,
        err: &str,
        tx: Option<mpsc::Sender<Value>>,
    ) {
        let changed = {
            let mut inner = self.inner.lock().expect("im_runtime mutex");
            let Some(entry) = inner.users.get_mut(user_id) else {
                return;
            };
            if entry.wecom_generation != gen_id {
                return;
            }
            entry.wecom_tx = tx;
            let same = entry.wecom_status.state == state && entry.wecom_status.last_error == err;
            entry.wecom_status = ChannelStatus {
                state: state.to_owned(),
                last_error: truncate_bytes(err, 240),
                updated_at: now_unix(),
            };
            !same
        };
        if changed {
            hub.events
                .emit(user_id, "im", json!({"channel": "wecom", "state": state}));
        }
    }

    fn set_dingtalk_state(&self, hub: &Hub, user_id: &str, gen_id: u64, state: &str, err: &str) {
        let changed = {
            let mut inner = self.inner.lock().expect("im_runtime mutex");
            let Some(entry) = inner.users.get_mut(user_id) else {
                return;
            };
            if entry.dingtalk_generation != gen_id {
                return;
            }
            let same =
                entry.dingtalk_status.state == state && entry.dingtalk_status.last_error == err;
            entry.dingtalk_status = ChannelStatus {
                state: state.to_owned(),
                last_error: truncate_bytes(err, 240),
                updated_at: now_unix(),
            };
            !same
        };
        if changed {
            hub.events.emit(
                user_id,
                "im",
                json!({"channel": "dingtalk", "state": state}),
            );
        }
    }

    fn get_statuses(&self, user_id: &str) -> (ChannelStatus, ChannelStatus) {
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner
            .users
            .get(user_id)
            .map(|u| (u.wecom_status.clone(), u.dingtalk_status.clone()))
            .unwrap_or_default()
    }

    fn get_any_wecom_tx(&self, preferred_uid: &str) -> Option<mpsc::Sender<Value>> {
        let inner = self.inner.lock().expect("im_runtime mutex");
        if let Some(tx) = inner
            .users
            .get(preferred_uid)
            .and_then(|u| u.wecom_tx.clone())
        {
            return Some(tx);
        }
        inner.users.values().find_map(|u| u.wecom_tx.clone())
    }

    fn has_any_wecom_connected(&self) -> bool {
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner.users.values().any(|u| u.wecom_tx.is_some())
    }

    fn is_dingtalk_connected(&self, user_id: &str) -> bool {
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner
            .users
            .get(user_id)
            .is_some_and(|u| u.dingtalk_status.state == "connected")
    }

    fn has_any_dingtalk_connected(&self) -> bool {
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner
            .users
            .values()
            .any(|u| u.dingtalk_status.state == "connected")
    }

    fn cache_dd_session_webhook(&self, user_id: &str, url: String, expires_at: i64) {
        let mut inner = self.inner.lock().expect("im_runtime mutex");
        let entry = inner.users.entry(user_id.to_owned()).or_default();
        entry.dd_session_webhook = Some((url, expires_at));
    }

    fn get_dd_session_webhook(&self, user_id: &str) -> Option<String> {
        let now = now_unix();
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner.users.get(user_id).and_then(|u| {
            u.dd_session_webhook
                .as_ref()
                .filter(|(_, exp)| *exp == 0 || *exp > now + 15)
                .map(|(url, _)| url.clone())
        })
    }

    fn get_dd_token(&self, user_id: &str) -> Option<String> {
        let now = now_unix();
        let inner = self.inner.lock().expect("im_runtime mutex");
        inner.users.get(user_id).and_then(|u| {
            u.dd_token
                .as_ref()
                .filter(|t| t.expires_at > now + 120)
                .map(|t| t.token.clone())
        })
    }

    fn cache_dd_token(&self, user_id: &str, token: String, expires_in_secs: i64) {
        let mut inner = self.inner.lock().expect("im_runtime mutex");
        let entry = inner.users.entry(user_id.to_owned()).or_default();
        entry.dd_token = Some(CachedToken {
            token,
            expires_at: now_unix() + expires_in_secs.max(60),
        });
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveWeComInput {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub ws_url: Option<String>,
    #[serde(default)]
    pub clear_bind: bool,
    #[serde(default)]
    pub clear: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveDingTalkInput {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
    #[serde(default)]
    pub clear_bind: bool,
    #[serde(default)]
    pub clear: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveImInput {
    #[serde(default)]
    pub wecom: Option<SaveWeComInput>,
    #[serde(default)]
    pub dingtalk: Option<SaveDingTalkInput>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub webhook_secret: Option<String>,
    #[serde(default)]
    pub clear_webhook: bool,
    #[serde(default)]
    pub clear_webhook_secret: bool,
    #[serde(default)]
    pub issue_bind_code: bool,
    #[serde(default)]
    pub notify_delegation: Option<bool>,
    #[serde(default)]
    pub notify_delivery: Option<bool>,
}

fn mask_preview(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let tail: String = trimmed
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("****{tail}")
}

fn generate_bind_code() -> String {
    let tok = random_token();
    tok.bytes()
        .take(6)
        .map(|b| BIND_ALPHABET[(b as usize) % BIND_ALPHABET.len()] as char)
        .collect()
}

fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len() * 3);
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn clean_im_text(raw: &str, is_group: bool) -> String {
    let mut s = raw.trim();
    if is_group {
        if let Some(after_at) = s.strip_prefix('@') {
            if let Some(ws) = after_at.find(char::is_whitespace) {
                s = &after_at[ws..];
            } else {
                return String::new();
            }
        }
    }
    s.trim().to_owned()
}

pub fn parse_bind_command(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let (prefix, rest) = trimmed.split_once(char::is_whitespace)?;
    if !prefix.eq_ignore_ascii_case("/bind") && prefix != "绑定" {
        return None;
    }
    let code = rest.trim();
    if code.len() == 6 && code.bytes().all(|b| b.is_ascii_alphanumeric()) {
        Some(code.to_ascii_uppercase())
    } else {
        None
    }
}

fn config_summary_json(
    cfg: &ImConfig,
    wecom_st: &ChannelStatus,
    dd_st: &ChannelStatus,
    runtime: &ImRuntime,
) -> Value {
    let now = now_unix();
    let bind_pending = !cfg.bind_code_hash.is_empty() && cfg.bind_code_expires_at > now;
    json!({
        "wecom": {
            "enabled": cfg.wecom.enabled,
            "bot_id": cfg.wecom.bot_id,
            "configured": !cfg.wecom.bot_id.is_empty() && !cfg.wecom.secret.is_empty(),
            "secret_preview": mask_preview(&cfg.wecom.secret),
            "bound_user_id": cfg.wecom.bound_user_id,
            "status": wecom_st,
        },
        "dingtalk": {
            "enabled": cfg.dingtalk.enabled,
            "client_id": cfg.dingtalk.client_id,
            "configured": !cfg.dingtalk.client_id.is_empty() && !cfg.dingtalk.client_secret.is_empty(),
            "secret_preview": mask_preview(&cfg.dingtalk.client_secret),
            "bound_staff_id": cfg.dingtalk.bound_staff_id,
            "status": dd_st,
        },
        "shared_wecom_online": runtime.has_any_wecom_connected(),
        "shared_dingtalk_online": runtime.has_any_dingtalk_connected(),
        "provider": cfg.provider,
        "configured": !cfg.webhook_url.is_empty(),
        "webhook_secret_configured": !cfg.webhook_secret.is_empty(),
        "bind_pending": bind_pending,
        "bind_expires_at": if bind_pending { cfg.bind_code_expires_at } else { 0 },
        "notify_delegation": cfg.notify_delegation,
        "notify_delivery": cfg.notify_delivery,
    })
}

async fn load_all_active_im_configs(hub: &Hub) -> Result<Vec<(String, String, String, ImConfig)>> {
    let secrets = hub.secrets.clone();
    hub.db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT u.id, u.username, u.display_name, i.config_json
                 FROM users u
                 JOIN integrations i ON i.user_id = u.id
                 WHERE u.status = 'active' AND i.kind = 'im'
                 ORDER BY u.created_at ASC",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::with_capacity(rows.len());
            for (uid, uname, dname, raw) in rows {
                if let Ok(cfg) = secrets.decrypt_json::<ImConfig>(SCOPE_IM, &uid, Some(raw)) {
                    out.push((uid, uname, dname, cfg));
                }
            }
            Ok(out)
        })
        .await
}

async fn load_im_config(hub: &Hub, user_id: &str) -> Result<ImConfig> {
    let uid = user_id.to_owned();
    let secrets = hub.secrets.clone();
    hub.db
        .read(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT config_json FROM integrations WHERE user_id = ?1 AND kind = 'im'",
                    [&uid],
                    |r| r.get(0),
                )
                .optional()?;
            secrets.decrypt_json(SCOPE_IM, &uid, raw)
        })
        .await
}

async fn save_im_config_db(
    hub: &Hub,
    user_id: &str,
    cfg: &ImConfig,
    audit_action: Option<(&str, &str)>,
) -> Result<()> {
    let uid = user_id.to_owned();
    let encoded = hub.secrets.encrypt_json(SCOPE_IM, user_id, cfg)?;
    let audit = audit_action.map(|(a, t)| (a.to_owned(), t.to_owned()));
    hub.db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO integrations(user_id, kind, config_json, updated_at)
                 VALUES (?1, 'im', ?2, ?3)
                 ON CONFLICT(user_id, kind) DO UPDATE SET config_json=excluded.config_json, updated_at=excluded.updated_at",
                params![uid, encoded, now_unix()],
            )?;
            if let Some((action, title)) = audit {
                record_audit(&tx, &uid, &action, "ok", &title, "")?;
            }
            tx.commit()?;
            Ok(())
        })
        .await
}

pub async fn get_config(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let cfg = load_im_config(&hub, &user.id).await?;
    let (wecom_st, dd_st) = hub.im_runtime.get_statuses(&user.id);
    Ok(Json(config_summary_json(
        &cfg,
        &wecom_st,
        &dd_st,
        &hub.im_runtime,
    )))
}

fn validate_custom_ws_url(raw: &str, allow_private: bool) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed != WECOM_DEFAULT_WS_URL && !allow_private {
        return Err(invalid("custom WeCom ws_url requires private outbound"));
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| invalid("invalid WeCom ws_url"))?;
    let scheme_ok = parsed.scheme() == "wss" || (allow_private && parsed.scheme() == "ws");
    if !scheme_ok || parsed.host_str().is_none() {
        return Err(invalid("invalid WeCom ws_url scheme or host"));
    }
    Ok(trimmed.to_owned())
}

fn validate_custom_api_base(raw: &str, allow_private: bool) -> Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed != DINGTALK_DEFAULT_API_BASE && !allow_private {
        return Err(invalid(
            "custom DingTalk api_base requires private outbound",
        ));
    }
    let parsed = reqwest::Url::parse(trimmed).map_err(|_| invalid("invalid DingTalk api_base"))?;
    let scheme_ok = parsed.scheme() == "https" || (allow_private && parsed.scheme() == "http");
    if !scheme_ok || parsed.host_str().is_none() {
        return Err(invalid("invalid DingTalk api_base scheme or host"));
    }
    Ok(trimmed.to_owned())
}

pub async fn save_config(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<SaveImInput>,
) -> Result<Json<Value>> {
    let mut cfg = load_im_config(&hub, &user.id).await?;
    let prev_wecom_enabled = cfg.wecom.enabled;
    let prev_dd_enabled = cfg.dingtalk.enabled;

    if let Some(ref provider_raw) = input.provider {
        let provider = provider_raw.trim().to_ascii_lowercase();
        if !matches!(
            provider.as_str(),
            "feishu" | "wecom" | "dingtalk" | "generic"
        ) {
            return Err(invalid(
                "provider must be feishu, wecom, dingtalk, or generic",
            ));
        }
        cfg.provider = provider;
    }

    if let Some(w) = input.wecom {
        if w.clear {
            cfg.wecom = WeComBotConfig::default();
        } else {
            if let Some(id) = w.bot_id {
                cfg.wecom.bot_id = truncate_bytes(id.trim(), 160);
            }
            if let Some(sec) = w.secret {
                let s = sec.trim();
                if !s.is_empty() {
                    cfg.wecom.secret = truncate_bytes(s, 256);
                }
            }
            if let Some(ws) = w.ws_url {
                cfg.wecom.ws_url = validate_custom_ws_url(&ws, hub.config.allow_private_outbound)?;
            }
            if w.clear_bind {
                cfg.wecom.bound_user_id.clear();
                cfg.wecom.push_chat_id.clear();
                cfg.wecom.push_chat_type = 1;
            }
            if let Some(en) = w.enabled {
                cfg.wecom.enabled = en;
            }
            if cfg.wecom.enabled && (cfg.wecom.bot_id.is_empty() || cfg.wecom.secret.is_empty()) {
                return Err(invalid("wecom bot_id and secret are required when enabled"));
            }
        }
    }

    if let Some(d) = input.dingtalk {
        if d.clear {
            cfg.dingtalk = DingTalkBotConfig::default();
        } else {
            if let Some(id) = d.client_id {
                cfg.dingtalk.client_id = truncate_bytes(id.trim(), 160);
            }
            if let Some(sec) = d.client_secret {
                let s = sec.trim();
                if !s.is_empty() {
                    cfg.dingtalk.client_secret = truncate_bytes(s, 256);
                }
            }
            if let Some(base) = d.api_base {
                cfg.dingtalk.api_base =
                    validate_custom_api_base(&base, hub.config.allow_private_outbound)?;
            }
            if d.clear_bind {
                cfg.dingtalk.bound_staff_id.clear();
                cfg.dingtalk.robot_code.clear();
            }
            if let Some(en) = d.enabled {
                cfg.dingtalk.enabled = en;
            }
            if cfg.dingtalk.enabled
                && (cfg.dingtalk.client_id.is_empty() || cfg.dingtalk.client_secret.is_empty())
            {
                return Err(invalid(
                    "dingtalk client_id and client_secret are required when enabled",
                ));
            }
        }
    }

    if input.clear_webhook {
        cfg.webhook_url.clear();
    } else if let Some(raw) = input
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let validated = validate_im_webhook_url(
            &cfg.provider,
            raw,
            hub.config.allow_generic_im_webhook,
            hub.config.allow_private_outbound,
        )
        .await
        .map_err(invalid)?;
        cfg.webhook_url = validated;
    }

    if input.clear_webhook_secret {
        cfg.webhook_secret.clear();
    } else if let Some(raw) = input
        .webhook_secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        cfg.webhook_secret = truncate_bytes(raw, 256);
    }

    if let Some(v) = input.notify_delegation {
        cfg.notify_delegation = v;
    }
    if let Some(v) = input.notify_delivery {
        cfg.notify_delivery = v;
    }

    let need_bind = (cfg.wecom.enabled && cfg.wecom.bound_user_id.is_empty())
        || (cfg.dingtalk.enabled && cfg.dingtalk.bound_staff_id.is_empty());
    let newly_unbound_enable =
        (!prev_wecom_enabled && cfg.wecom.enabled && cfg.wecom.bound_user_id.is_empty())
            || (!prev_dd_enabled && cfg.dingtalk.enabled && cfg.dingtalk.bound_staff_id.is_empty());
    let mut issued_bind_code: Option<String> = None;
    if input.issue_bind_code
        || (need_bind
            && (newly_unbound_enable
                || cfg.bind_code_hash.is_empty()
                || cfg.bind_code_expires_at <= now_unix()))
    {
        let code = generate_bind_code();
        cfg.bind_code_hash = sha256_hex(&code);
        cfg.bind_code_expires_at = now_unix() + BIND_CODE_TTL_SECS;
        issued_bind_code = Some(code);
    }

    save_im_config_db(
        &hub,
        &user.id,
        &cfg,
        Some(("im_config", "更新 IM 接入配置")),
    )
    .await?;
    hub.im_runtime.sync_user(&hub, &user.id).await;
    hub.events.emit(&user.id, "im", json!({"updated": true}));

    let (wecom_st, dd_st) = hub.im_runtime.get_statuses(&user.id);
    let mut out = config_summary_json(&cfg, &wecom_st, &dd_st, &hub.im_runtime);
    out["ok"] = json!(true);
    out["bind_code"] = json!(issued_bind_code);
    Ok(Json(out))
}

fn sign_webhook_request(
    provider: &str,
    url: &str,
    secret: &str,
    mut body: Value,
) -> (String, Value) {
    let sec = secret.trim();
    if sec.is_empty() {
        return (url.to_owned(), body);
    }
    match provider {
        "dingtalk" => {
            let ts_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let to_sign = format!("{ts_ms}\n{sec}");
            let key = hmac::Key::new(hmac::HMAC_SHA256, sec.as_bytes());
            let sig = STANDARD.encode(hmac::sign(&key, to_sign.as_bytes()).as_ref());
            let sep = if url.contains('?') { '&' } else { '?' };
            let signed_url = format!("{url}{sep}timestamp={ts_ms}&sign={}", percent_encode(&sig));
            (signed_url, body)
        }
        "feishu" => {
            let ts_str = now_unix().to_string();
            let key_str = format!("{ts_str}\n{sec}");
            let key = hmac::Key::new(hmac::HMAC_SHA256, key_str.as_bytes());
            let sig = STANDARD.encode(hmac::sign(&key, b"").as_ref());
            if let Some(map) = body.as_object_mut() {
                map.insert("timestamp".into(), json!(ts_str));
                map.insert("sign".into(), json!(sig));
            }
            (url.to_owned(), body)
        }
        _ => (url.to_owned(), body),
    }
}

fn build_card_payload(provider: &str, title: &str, content: &str) -> Value {
    let body = truncate_bytes(content.trim(), 1500);
    match provider {
        "feishu" => json!({
            "msg_type": "interactive",
            "card": {
                "header": {"title": {"tag": "plain_text", "content": title}},
                "elements": [{"tag": "markdown", "content": body}]
            }
        }),
        "wecom" => json!({
            "msg_type": "markdown",
            "markdown": {"content": format!("### {title}\n{body}")}
        }),
        "dingtalk" => json!({
            "msgtype": "markdown",
            "markdown": {"title": title, "text": format!("### {title}\n{body}")}
        }),
        _ => json!({"title": title, "content": body}),
    }
}

async fn post_group_webhook(
    hub: &Hub,
    cfg: &ImConfig,
    title: &str,
    content: &str,
) -> std::result::Result<(), &'static str> {
    if cfg.webhook_url.is_empty() {
        return Err("webhook_url empty");
    }
    validate_im_webhook_url(
        &cfg.provider,
        &cfg.webhook_url,
        hub.config.allow_generic_im_webhook,
        hub.config.allow_private_outbound,
    )
    .await
    .map_err(|_| "invalid webhook_url")?;

    let payload = build_card_payload(&cfg.provider, title, content);
    let (target_url, signed_payload) = sign_webhook_request(
        &cfg.provider,
        &cfg.webhook_url,
        &cfg.webhook_secret,
        payload,
    );
    let resp = hub
        .user_http
        .post(&target_url)
        .json(&signed_payload)
        .send()
        .await
        .map_err(|_| "IM webhook unreachable")?;
    if !resp.status().is_success() {
        return Err("IM webhook returned non-2xx");
    }
    if let Ok(json_body) = resp.json::<Value>().await {
        let code = json_body
            .get("errcode")
            .or_else(|| json_body.get("code"))
            .and_then(Value::as_i64);
        if code.is_some_and(|c| c != 0) {
            return Err("IM webhook returned non-zero error code");
        }
    }
    Ok(())
}

async fn send_wecom_proactive(
    hub: &Hub,
    user_id: &str,
    cfg: &WeComBotConfig,
    title: &str,
    content: &str,
) -> std::result::Result<(), &'static str> {
    if cfg.bound_user_id.is_empty() {
        return Err("wecom not bound");
    }
    let tx = hub
        .im_runtime
        .get_any_wecom_tx(user_id)
        .ok_or("wecom websocket disconnected")?;
    let body_md = truncate_bytes(&format!("**{title}**\n\n{}", content.trim()), 1800);
    // Always deliver personal proactive alerts (task results, delegations, /approve codes) 1v1.
    let frame = json!({
        "cmd": "aibot_send_msg",
        "headers": {
            "req_id": format!("push_{}", Uuid::new_v4().simple()),
        },
        "body": {
            "chatid": cfg.bound_user_id,
            "chat_type": 1,
            "msgtype": "markdown",
            "markdown": {"content": body_md},
        }
    });
    tx.send(frame).await.map_err(|_| "wecom channel closed")
}

async fn fetch_dingtalk_token(
    hub: &Hub,
    user_id: &str,
    cfg: &DingTalkBotConfig,
) -> std::result::Result<String, &'static str> {
    if let Some(tok) = hub.im_runtime.get_dd_token(user_id) {
        return Ok(tok);
    }
    let api_base = resolved_dd_api_base(cfg, hub.config.allow_private_outbound);
    if !hub.config.allow_private_outbound {
        validate_outbound_host(
            "api.dingtalk.com",
            443,
            "dingtalk IP blocked",
            "dingtalk DNS failed",
            "dingtalk resolves to private IP",
        )
        .await
        .map_err(|_| "dingtalk host validation failed")?;
    }
    let url = format!("{api_base}/v1.0/oauth2/accessToken");
    let resp = hub
        .user_http
        .post(&url)
        .json(&json!({
            "appKey": cfg.client_id,
            "appSecret": cfg.client_secret,
        }))
        .send()
        .await
        .map_err(|_| "dingtalk token request failed")?;
    if !resp.status().is_success() {
        return Err("dingtalk token HTTP error");
    }
    let data: Value = resp
        .json()
        .await
        .map_err(|_| "dingtalk token invalid JSON")?;
    let token = data["accessToken"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or("dingtalk missing accessToken")?
        .to_owned();
    let exp = data["expireIn"].as_i64().unwrap_or(7200);
    hub.im_runtime.cache_dd_token(user_id, token.clone(), exp);
    Ok(token)
}

async fn send_dingtalk_session_webhook(
    hub: &Hub,
    webhook_url: &str,
    title: &str,
    content: &str,
) -> std::result::Result<(), &'static str> {
    let parsed = reqwest::Url::parse(webhook_url).map_err(|_| "invalid dingtalk sessionWebhook")?;
    if !hub.config.allow_private_outbound {
        let host = parsed
            .host_str()
            .ok_or("missing sessionWebhook host")?
            .to_ascii_lowercase();
        if parsed.scheme() != "https" || (host != "oapi.dingtalk.com" && host != "api.dingtalk.com")
        {
            return Err("disallowed dingtalk sessionWebhook host");
        }
        validate_outbound_host(
            &host,
            parsed.port_or_known_default().unwrap_or(443),
            "dingtalk webhook IP blocked",
            "dingtalk webhook DNS failed",
            "dingtalk webhook resolves to private IP",
        )
        .await
        .map_err(|_| "dingtalk sessionWebhook host blocked")?;
    }
    let body_md = truncate_bytes(&format!("### {title}\n{}", content.trim()), 1800);
    let resp = hub
        .user_http
        .post(webhook_url)
        .json(&json!({
            "msgtype": "markdown",
            "markdown": {"title": title, "text": body_md},
        }))
        .send()
        .await
        .map_err(|_| "dingtalk sessionWebhook unreachable")?;
    if !resp.status().is_success() {
        return Err("dingtalk sessionWebhook returned non-2xx");
    }
    if let Ok(v) = resp.json::<Value>().await {
        if v.get("errcode")
            .and_then(Value::as_i64)
            .is_some_and(|c| c != 0)
        {
            return Err("dingtalk sessionWebhook returned errcode");
        }
    }
    Ok(())
}

async fn resolve_dingtalk_bot_for_push(
    hub: &Hub,
    user_id: &str,
    user_cfg: &DingTalkBotConfig,
) -> Option<(String, DingTalkBotConfig)> {
    if user_cfg.enabled && !user_cfg.client_id.is_empty() && !user_cfg.client_secret.is_empty() {
        return Some((user_id.to_owned(), user_cfg.clone()));
    }
    let all = load_all_active_im_configs(hub).await.ok()?;
    let mut fallback = None;
    for (uid, _, _, cfg) in all {
        if cfg.dingtalk.enabled
            && !cfg.dingtalk.client_id.is_empty()
            && !cfg.dingtalk.client_secret.is_empty()
        {
            if hub.im_runtime.is_dingtalk_connected(&uid) {
                return Some((uid, cfg.dingtalk));
            }
            if fallback.is_none() {
                fallback = Some((uid, cfg.dingtalk));
            }
        }
    }
    fallback
}

async fn send_dingtalk_proactive(
    hub: &Hub,
    user_id: &str,
    cfg: &DingTalkBotConfig,
    title: &str,
    content: &str,
) -> std::result::Result<(), &'static str> {
    if cfg.bound_staff_id.is_empty() {
        return Err("dingtalk not bound");
    }
    if let Some((bot_owner_uid, bot_cfg)) = resolve_dingtalk_bot_for_push(hub, user_id, cfg).await {
        if let Ok(token) = fetch_dingtalk_token(hub, &bot_owner_uid, &bot_cfg).await {
            let api_base = resolved_dd_api_base(&bot_cfg, hub.config.allow_private_outbound);
            let robot_code = if !cfg.robot_code.is_empty() {
                cfg.robot_code.as_str()
            } else if !bot_cfg.robot_code.is_empty() {
                bot_cfg.robot_code.as_str()
            } else {
                bot_cfg.client_id.as_str()
            };
            let msg_param = json!({
                "title": truncate_bytes(title, 120),
                "text": truncate_bytes(content.trim(), 1800),
            })
            .to_string();
            if let Ok(resp) = hub
                .user_http
                .post(format!("{api_base}/v1.0/robot/oToMessages/batchSend"))
                .header("x-acs-dingtalk-access-token", token)
                .json(&json!({
                    "robotCode": robot_code,
                    "userIds": [cfg.bound_staff_id],
                    "msgKey": "sampleMarkdown",
                    "msgParam": msg_param,
                }))
                .send()
                .await
            {
                if resp.status().is_success() {
                    return Ok(());
                }
            }
        }
    }
    if let Some(hook) = hub.im_runtime.get_dd_session_webhook(user_id) {
        return send_dingtalk_session_webhook(hub, &hook, title, content).await;
    }
    Err("dingtalk proactive push failed")
}

pub async fn maybe_notify(hub: &Hub, user_id: &str, event_kind: &str, title: &str, content: &str) {
    let Ok(cfg) = load_im_config(hub, user_id).await else {
        return;
    };
    let enabled = match event_kind {
        "delegation" => cfg.notify_delegation,
        "delivery" => cfg.notify_delivery,
        _ => true,
    };
    if !enabled {
        return;
    }
    if !cfg.wecom.bound_user_id.is_empty() {
        let _ = send_wecom_proactive(hub, user_id, &cfg.wecom, title, content).await;
    }
    if !cfg.dingtalk.bound_staff_id.is_empty() {
        let _ = send_dingtalk_proactive(hub, user_id, &cfg.dingtalk, title, content).await;
    }
    if !cfg.webhook_url.is_empty() {
        let _ = post_group_webhook(hub, &cfg, title, content).await;
    }
}

pub async fn test_webhook(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let cfg = load_im_config(&hub, &user.id).await?;
    let has_wecom = !cfg.wecom.bound_user_id.is_empty();
    let has_dd = !cfg.dingtalk.bound_staff_id.is_empty();
    let has_hook = !cfg.webhook_url.is_empty();
    if !has_wecom && !has_dd && !has_hook {
        return Err(invalid(
            "请先配置并绑定企业微信/钉钉长连接，或填写群通知 Webhook URL",
        ));
    }
    let title = "Agenct 通知通道测试";
    let body = format!("账号 `{}` 的实时双向通知已连通。", user.username);
    let mut delivered = Vec::new();
    let mut last_err: Option<&'static str> = None;

    if has_wecom {
        match send_wecom_proactive(&hub, &user.id, &cfg.wecom, title, &body).await {
            Ok(()) => delivered.push("wecom"),
            Err(e) => last_err = Some(e),
        }
    }
    if has_dd {
        match send_dingtalk_proactive(&hub, &user.id, &cfg.dingtalk, title, &body).await {
            Ok(()) => delivered.push("dingtalk"),
            Err(e) => last_err = Some(e),
        }
    }
    if has_hook {
        match post_group_webhook(&hub, &cfg, title, &body).await {
            Ok(()) => delivered.push("webhook"),
            Err(e) => last_err = Some(e),
        }
    }
    if delivered.is_empty() {
        return Err(Error(
            StatusCode::BAD_GATEWAY,
            last_err.unwrap_or("IM channel test failed"),
        ));
    }
    Ok(Json(json!({"ok": true, "delivered": delivered})))
}

#[derive(Clone, Copy)]
enum ImChannel {
    WeCom,
    DingTalk,
}

enum RouteDecision {
    DirectReply(String),
    Routed { user_id: String, text: String },
    Ignore,
}

struct InboundMeta<'a> {
    channel: ImChannel,
    sender_id: &'a str,
    is_group: bool,
    robot_code: &'a str,
}

async fn resolve_or_bind_im_sender(
    hub: &Hub,
    meta: InboundMeta<'_>,
    raw_text: &str,
) -> RouteDecision {
    let text = clean_im_text(raw_text, meta.is_group);
    if text.is_empty() {
        return RouteDecision::Ignore;
    }
    let ch_name = match meta.channel {
        ImChannel::WeCom => "wecom",
        ImChannel::DingTalk => "dingtalk",
    };
    let Ok(all_cfgs) = load_all_active_im_configs(hub).await else {
        return RouteDecision::Ignore;
    };

    if let Some(code) = parse_bind_command(&text) {
        let rate_key = format!("im_bind:{ch_name}:{}", meta.sender_id);
        if !hub.approval_limiter.check_allowed(&rate_key) {
            return RouteDecision::DirectReply("尝试次数过多，请 10 分钟后再试。".into());
        }
        let hash = sha256_hex(&code);
        let now = now_unix();
        let matched = all_cfgs.iter().find(|(_, _, _, cfg)| {
            !cfg.bind_code_hash.is_empty()
                && cfg.bind_code_expires_at >= now
                && constant_time_eq(cfg.bind_code_hash.as_bytes(), hash.as_bytes())
        });
        let Some((matched_uid, matched_uname, matched_dname, matched_cfg_ref)) = matched else {
            hub.approval_limiter.record_failure(&rate_key);
            return RouteDecision::DirectReply(
                "配对口令无效或已过期，请在网页「设置 → IM 随身双向接入」重新生成口令。".into(),
            );
        };
        if let Some((_, other_uname, _, _)) = all_cfgs.iter().find(|(uid, _, _, cfg)| {
            uid != matched_uid
                && match meta.channel {
                    ImChannel::WeCom => cfg.wecom.bound_user_id == meta.sender_id,
                    ImChannel::DingTalk => cfg.dingtalk.bound_staff_id == meta.sender_id,
                }
        }) {
            return RouteDecision::DirectReply(format!(
                "该账号已绑定至 @{other_uname}，如需切换请先在原账号设置中解绑。"
            ));
        }

        hub.approval_limiter.reset(&rate_key);
        let mut updated = matched_cfg_ref.clone();
        match meta.channel {
            ImChannel::WeCom => {
                updated.wecom.bound_user_id = meta.sender_id.to_owned();
                updated.wecom.push_chat_id = meta.sender_id.to_owned();
                updated.wecom.push_chat_type = 1;
            }
            ImChannel::DingTalk => {
                updated.dingtalk.bound_staff_id = meta.sender_id.to_owned();
                if !meta.robot_code.trim().is_empty() {
                    updated.dingtalk.robot_code = meta.robot_code.trim().to_owned();
                }
            }
        }
        let wecom_pending = updated.wecom.bound_user_id.is_empty()
            && (updated.wecom.enabled || hub.im_runtime.has_any_wecom_connected());
        let dd_pending = updated.dingtalk.bound_staff_id.is_empty()
            && (updated.dingtalk.enabled || hub.im_runtime.has_any_dingtalk_connected());
        if !wecom_pending && !dd_pending {
            updated.bind_code_hash.clear();
            updated.bind_code_expires_at = 0;
        }
        if save_im_config_db(
            hub,
            matched_uid,
            &updated,
            Some((
                "im_bind",
                &format!("绑定 {ch_name} 账号 {}", meta.sender_id),
            )),
        )
        .await
        .is_err()
        {
            return RouteDecision::DirectReply("保存绑定状态失败。".into());
        }
        hub.events.emit(
            matched_uid,
            "im",
            json!({"bound": ch_name, "sender_id": meta.sender_id}),
        );
        return RouteDecision::DirectReply(format!(
            "已成功绑定到 Agenct 账号「{matched_dname} (@{matched_uname})」（{ch_name}: {}）。现在单聊或在群里 @我，都会进入你自己的专属智能体空间，也可以直接发送 `/approve <审批码>` 放行任务。",
            meta.sender_id
        ));
    }

    let owner = all_cfgs
        .into_iter()
        .find(|(_, _, _, cfg)| match meta.channel {
            ImChannel::WeCom => cfg.wecom.bound_user_id == meta.sender_id,
            ImChannel::DingTalk => cfg.dingtalk.bound_staff_id == meta.sender_id,
        });
    match owner {
        Some((uid, _, _, _)) => {
            if text.len() > 4000 {
                return RouteDecision::DirectReply("消息过长，请控制在 4000 字节以内。".into());
            }
            RouteDecision::Routed { user_id: uid, text }
        }
        None => RouteDecision::DirectReply(format!(
            "你尚未绑定个人 Agenct 账号：请先登录 {} ，在「设置 → IM 随身双向接入」点击「生成配对口令」，再发送 `/bind <6位口令>` 即可启用你自己的专属智能体。",
            hub.config.public_origin
        )),
    }
}

fn resolved_wecom_ws_url(cfg: &WeComBotConfig, allow_private: bool) -> String {
    if allow_private && !cfg.ws_url.trim().is_empty() {
        cfg.ws_url.trim().to_owned()
    } else {
        WECOM_DEFAULT_WS_URL.to_owned()
    }
}

fn resolved_dd_api_base(cfg: &DingTalkBotConfig, allow_private: bool) -> String {
    if allow_private && !cfg.api_base.trim().is_empty() {
        cfg.api_base.trim().trim_end_matches('/').to_owned()
    } else {
        DINGTALK_DEFAULT_API_BASE.to_owned()
    }
}

fn dedup_seen(recent: &mut VecDeque<String>, seen: &mut HashSet<String>, id: &str) -> bool {
    if id.is_empty() || seen.contains(id) {
        return false;
    }
    if recent.len() >= 128 {
        if let Some(old) = recent.pop_front() {
            seen.remove(&old);
        }
    }
    recent.push_back(id.to_owned());
    seen.insert(id.to_owned());
    true
}

fn parse_wecom_msg_text(body: &Value) -> Option<String> {
    let msgtype = body["msgtype"].as_str().unwrap_or("");
    match msgtype {
        "text" => body["text"]["content"].as_str().map(str::to_owned),
        "mixed" => {
            let items = body["mixed"]["msg_item"].as_array()?;
            let mut parts = Vec::new();
            for item in items {
                if item["msgtype"].as_str() == Some("text") {
                    if let Some(t) = item["text"]["content"].as_str() {
                        let clean = t.trim();
                        if !clean.is_empty() {
                            parts.push(clean);
                        }
                    }
                }
            }
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => body["text"]["content"].as_str().map(str::to_owned),
    }
}

fn wecom_stream_frame(req_id: &str, stream_id: &str, content: &str, finish: bool) -> Value {
    json!({
        "cmd": "aibot_respond_msg",
        "headers": {"req_id": req_id},
        "body": {
            "msgtype": "stream",
            "stream": {
                "id": stream_id,
                "content": truncate_bytes(content, 4000),
                "finish": finish,
            }
        }
    })
}

async fn run_wecom_supervisor(hub: Hub, user_id: String, gen_id: u64, cfg: WeComBotConfig) {
    let mut backoff = Duration::from_secs(2);
    loop {
        let connected = run_wecom_once(&hub, &user_id, gen_id, &cfg).await;
        match connected {
            Ok(()) => backoff = Duration::from_secs(2),
            Err(err_msg) => {
                hub.im_runtime
                    .set_wecom_state(&hub, &user_id, gen_id, "error", &err_msg, None);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run_wecom_once(
    hub: &Hub,
    user_id: &str,
    gen_id: u64,
    cfg: &WeComBotConfig,
) -> std::result::Result<(), String> {
    let ws_url = resolved_wecom_ws_url(cfg, hub.config.allow_private_outbound);
    if !hub.config.allow_private_outbound {
        validate_outbound_host(
            "openws.work.weixin.qq.com",
            443,
            "wecom endpoint blocked",
            "wecom DNS failed",
            "wecom resolves to private IP",
        )
        .await
        .map_err(str::to_owned)?;
    }

    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(15), connect_async(&ws_url))
        .await
        .map_err(|_| "连接企业微信长连接超时".to_owned())?
        .map_err(|e| format!("企业微信长连接握手失败: {e}"))?;

    let sub_req = json!({
        "cmd": "aibot_subscribe",
        "headers": {"req_id": format!("sub_{}", Uuid::new_v4().simple())},
        "body": {
            "bot_id": cfg.bot_id,
            "secret": cfg.secret,
        }
    });
    ws.send(Message::Text(sub_req.to_string().into()))
        .await
        .map_err(|e| format!("发送企微鉴权失败: {e}"))?;

    let first_frame = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .map_err(|_| "企业微信鉴权超时".to_owned())?
        .ok_or_else(|| "企业微信连接已关闭".to_owned())?
        .map_err(|e| format!("读取企微鉴权响应失败: {e}"))?;
    let auth_text = match first_frame {
        Message::Text(t) => t.to_string(),
        Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
        _ => return Err("企业微信鉴权响应帧格式异常".into()),
    };
    let auth_json: Value =
        serde_json::from_str(&auth_text).map_err(|_| "企业微信鉴权响应非 JSON".to_owned())?;
    let errcode = auth_json
        .get("errcode")
        .or_else(|| auth_json["body"].get("errcode"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if errcode != 0 {
        let errmsg = auth_json["errmsg"]
            .as_str()
            .or_else(|| auth_json["body"]["errmsg"].as_str())
            .unwrap_or("auth failed");
        return Err(format!("企业微信鉴权拒绝 ({errcode}): {errmsg}"));
    }

    let (tx, mut rx) = mpsc::channel::<Value>(32);
    hub.im_runtime
        .set_wecom_state(hub, user_id, gen_id, "connected", "", Some(tx.clone()));

    let (mut sink, mut stream) = ws.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(25));
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    heartbeat.tick().await;
    let mut recent_ids = VecDeque::with_capacity(128);
    let mut seen_ids = HashSet::with_capacity(128);

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                let ping = json!({"cmd": "ping", "headers": {"req_id": format!("ping_{}", now_unix())}});
                if sink.send(Message::Text(ping.to_string().into())).await.is_err() {
                    return Err("企业微信心跳发送失败".into());
                }
            }
            Some(out) = rx.recv() => {
                if sink.send(Message::Text(out.to_string().into())).await.is_err() {
                    return Err("企业微信下发通道中断".into());
                }
            }
            frame = tokio::time::timeout(Duration::from_secs(75), stream.next()) => {
                let Some(item) = frame.map_err(|_| "企业微信读超时，准备重连".to_owned())? else {
                    return Err("企业微信服务端断开连接".into());
                };
                let raw = match item.map_err(|e| format!("企业微信收帧异常: {e}"))? {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Ping(p) => {
                        let _ = sink.send(Message::Pong(p)).await;
                        continue;
                    }
                    Message::Close(_) => return Err("企业微信连接关闭".into()),
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                if v["cmd"].as_str() != Some("aibot_msg_callback") {
                    continue;
                }
                let req_id = v["headers"]["req_id"].as_str().unwrap_or("").trim().to_owned();
                let body = &v["body"];
                let msgid = body["msgid"].as_str().unwrap_or("").trim().to_owned();
                if req_id.is_empty() || !dedup_seen(&mut recent_ids, &mut seen_ids, &msgid) {
                    continue;
                }
                let Some(raw_text) = parse_wecom_msg_text(body) else {
                    let stream_id = Uuid::new_v4().simple().to_string();
                    let _ = tx.send(wecom_stream_frame(&req_id, &stream_id, "暂仅支持文本消息。", true)).await;
                    continue;
                };
                let chattype = body["chattype"].as_str().unwrap_or("single");
                let is_group = chattype == "group";
                let sender_id = body["from"]["userid"].as_str().unwrap_or("").trim().to_owned();
                if sender_id.is_empty() {
                    continue;
                }

                let hub_c = hub.clone();
                let tx_c = tx.clone();
                tokio::spawn(async move {
                    handle_wecom_callback(hub_c, tx_c, req_id, msgid, sender_id, is_group, raw_text)
                        .await;
                });
            }
        }
    }
}

async fn handle_wecom_callback(
    hub: Hub,
    tx: mpsc::Sender<Value>,
    req_id: String,
    msgid: String,
    sender_id: String,
    is_group: bool,
    raw_text: String,
) {
    let stream_id = format!("st_{}", Uuid::new_v4().simple());
    let (target_uid, prompt_text) = match resolve_or_bind_im_sender(
        &hub,
        InboundMeta {
            channel: ImChannel::WeCom,
            sender_id: &sender_id,
            is_group,
            robot_code: "",
        },
        &raw_text,
    )
    .await
    {
        RouteDecision::DirectReply(msg) => {
            let _ = tx
                .send(wecom_stream_frame(&req_id, &stream_id, &msg, true))
                .await;
            return;
        }
        RouteDecision::Ignore => return,
        RouteDecision::Routed { user_id, text } => (user_id, text),
    };

    let mut ev_rx = hub.events.subscribe(&target_uid);
    let _ = tx
        .send(wecom_stream_frame(&req_id, &stream_id, "思考中…", false))
        .await;

    let turn_req_id = format!("wecom:{msgid}");
    let hub_turn = hub.clone();
    let uid_turn = target_uid.clone();
    let mut turn_handle = tokio::spawn(async move {
        run_turn(&hub_turn, &uid_turn, &prompt_text, Some(&turn_req_id), None).await
    });

    let mut acc = String::new();
    let mut last_flush = Instant::now();
    let final_reply = loop {
        tokio::select! {
            res = &mut turn_handle => {
                break match res {
                    Ok(Ok(val)) => val["reply"].as_str().unwrap_or("好的，已处理。").to_owned(),
                    Ok(Err(err)) => format!("处理失败：{}", err.1),
                    Err(_) => "处理异常，请稍后重试。".to_owned(),
                };
            }
            ev = ev_rx.recv() => {
                let Ok(ev) = ev else { continue };
                if ev.kind == "delta" {
                    if let Ok(d) = serde_json::from_str::<Value>(&ev.data) {
                        if d["reset"].as_bool() == Some(true) {
                            acc.clear();
                        } else if let Some(piece) = d["text"].as_str() {
                            acc.push_str(piece);
                            if !acc.trim().is_empty() && last_flush.elapsed() >= Duration::from_millis(450) {
                                let _ = tx.send(wecom_stream_frame(&req_id, &stream_id, &acc, false)).await;
                                last_flush = Instant::now();
                            }
                        }
                    }
                } else if ev.kind == "stage" && acc.is_empty() {
                    if let Ok(d) = serde_json::from_str::<Value>(&ev.data) {
                        if let Some(st) = d["stage"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                            if last_flush.elapsed() >= Duration::from_millis(450) {
                                let _ = tx.send(wecom_stream_frame(&req_id, &stream_id, st, false)).await;
                                last_flush = Instant::now();
                            }
                        }
                    }
                }
            }
        }
    };

    let _ = tx
        .send(wecom_stream_frame(&req_id, &stream_id, &final_reply, true))
        .await;
}

async fn run_dingtalk_supervisor(hub: Hub, user_id: String, gen_id: u64, cfg: DingTalkBotConfig) {
    let mut backoff = Duration::from_secs(2);
    loop {
        match run_dingtalk_once(&hub, &user_id, gen_id, &cfg).await {
            Ok(()) => backoff = Duration::from_secs(2),
            Err(err_msg) => {
                hub.im_runtime
                    .set_dingtalk_state(&hub, &user_id, gen_id, "error", &err_msg);
            }
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

async fn run_dingtalk_once(
    hub: &Hub,
    user_id: &str,
    gen_id: u64,
    cfg: &DingTalkBotConfig,
) -> std::result::Result<(), String> {
    let api_base = resolved_dd_api_base(cfg, hub.config.allow_private_outbound);
    if !hub.config.allow_private_outbound {
        validate_outbound_host(
            "api.dingtalk.com",
            443,
            "dingtalk endpoint blocked",
            "dingtalk DNS failed",
            "dingtalk resolves to private IP",
        )
        .await
        .map_err(str::to_owned)?;
    }

    let open_resp = hub
        .user_http
        .post(format!("{api_base}/v1.0/gateway/connections/open"))
        .json(&json!({
            "clientId": cfg.client_id,
            "clientSecret": cfg.client_secret,
            "subscriptions": [
                {"type": "EVENT", "topic": "*"},
                {"type": "CALLBACK", "topic": "/v1.0/im/bot/messages/get"},
            ],
            "ua": "agenct-hub/2.0",
        }))
        .send()
        .await
        .map_err(|e| format!("钉钉网关注册失败: {e}"))?;
    if !open_resp.status().is_success() {
        return Err(format!("钉钉网关返回状态 {}", open_resp.status()));
    }
    let ticket_json: Value = open_resp
        .json()
        .await
        .map_err(|_| "钉钉网关返回非 JSON".to_owned())?;
    let endpoint = ticket_json["endpoint"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "钉钉未返回 WebSocket endpoint".to_owned())?;
    let ticket = ticket_json["ticket"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "钉钉未返回连接 ticket".to_owned())?;

    let ep_url = reqwest::Url::parse(endpoint).map_err(|_| "钉钉 endpoint URL 无效".to_owned())?;
    if !hub.config.allow_private_outbound {
        let host = ep_url
            .host_str()
            .ok_or_else(|| "钉钉 endpoint 缺少域名".to_owned())?
            .to_ascii_lowercase();
        if ep_url.scheme() != "wss" || (!host.ends_with(".dingtalk.com") && host != "dingtalk.com")
        {
            return Err("钉钉返回的 WebSocket 地址不在白名单域名内".into());
        }
        validate_outbound_host(
            &host,
            ep_url.port_or_known_default().unwrap_or(443),
            "dingtalk ws IP blocked",
            "dingtalk ws DNS failed",
            "dingtalk ws resolves to private IP",
        )
        .await
        .map_err(str::to_owned)?;
    }

    let sep = if endpoint.contains('?') { '&' } else { '?' };
    let ws_url = format!("{endpoint}{sep}ticket={}", percent_encode(ticket));

    let (mut ws, _) = tokio::time::timeout(Duration::from_secs(15), connect_async(&ws_url))
        .await
        .map_err(|_| "连接钉钉 Stream 网关超时".to_owned())?
        .map_err(|e| format!("钉钉 Stream 握手失败: {e}"))?;

    hub.im_runtime
        .set_dingtalk_state(hub, user_id, gen_id, "connected", "");

    let mut keepalive = tokio::time::interval(Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    keepalive.tick().await;
    let mut recent_ids = VecDeque::with_capacity(128);
    let mut seen_ids = HashSet::with_capacity(128);

    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return Err("钉钉心跳发送失败".into());
                }
            }
            frame = tokio::time::timeout(Duration::from_secs(75), ws.next()) => {
                let Some(item) = frame.map_err(|_| "钉钉 Stream 读超时，准备重连".to_owned())? else {
                    return Err("钉钉 Stream 连接已关闭".into());
                };
                let raw = match item.map_err(|e| format!("钉钉收帧异常: {e}"))? {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Ping(p) => {
                        let _ = ws.send(Message::Pong(p)).await;
                        continue;
                    }
                    Message::Close(_) => return Err("钉钉 Stream 连接关闭".into()),
                    _ => continue,
                };
                let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let frame_type = v["type"].as_str().unwrap_or("");
                let topic = v["headers"]["topic"].as_str().unwrap_or("");
                let message_id = v["headers"]["messageId"].as_str().unwrap_or("").to_owned();

                if frame_type == "SYSTEM" && topic == "ping" {
                    let data_echo = match &v["data"] {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    };
                    let pong = json!({
                        "code": 200,
                        "message": "OK",
                        "headers": {
                            "messageId": message_id,
                            "contentType": "application/json",
                        },
                        "data": data_echo,
                    });
                    if ws.send(Message::Text(pong.to_string().into())).await.is_err() {
                        return Err("钉钉 ping 回执失败".into());
                    }
                    continue;
                }
                if frame_type == "SYSTEM" && topic == "disconnect" {
                    return Ok(());
                }
                if frame_type == "CALLBACK" && topic == "/v1.0/im/bot/messages/get" {
                    // Always ACK immediately so DingTalk does not retry long-running LLM turns.
                    let ack = json!({
                        "code": 200,
                        "message": "OK",
                        "headers": {
                            "messageId": message_id,
                            "contentType": "application/json",
                        },
                        "data": "{\"status\":\"SUCCESS\",\"message\":\"OK\"}",
                    });
                    if ws.send(Message::Text(ack.to_string().into())).await.is_err() {
                        return Err("钉钉消息 ACK 失败".into());
                    }

                    let data: Value = match &v["data"] {
                        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                        other => other.clone(),
                    };
                    let dedup_id = data["msgId"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&message_id)
                        .to_owned();
                    if !dedup_seen(&mut recent_ids, &mut seen_ids, &dedup_id) {
                        continue;
                    }
                    let sender_id = data["senderStaffId"]
                        .as_str()
                        .or_else(|| data["senderId"].as_str())
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    if sender_id.is_empty() {
                        continue;
                    }
                    let raw_text = data["text"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .trim()
                        .to_owned();
                    let is_group = data["conversationType"].as_str() == Some("2");
                    let robot_code = data["robotCode"].as_str().unwrap_or("").trim().to_owned();
                    let session_webhook =
                        data["sessionWebhook"].as_str().unwrap_or("").trim().to_owned();
                    let exp_sec = data["sessionWebhookExpiredTime"]
                        .as_i64()
                        .map(|ms| if ms > 10_000_000_000 { ms / 1000 } else { ms })
                        .unwrap_or(0);

                    let hub_c = hub.clone();
                    tokio::spawn(async move {
                        handle_dingtalk_callback(
                            hub_c,
                            dedup_id,
                            sender_id,
                            robot_code,
                            is_group,
                            session_webhook,
                            exp_sec,
                            raw_text,
                        )
                        .await;
                    });
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_dingtalk_callback(
    hub: Hub,
    dedup_id: String,
    sender_id: String,
    robot_code: String,
    is_group: bool,
    session_webhook: String,
    exp_sec: i64,
    raw_text: String,
) {
    let (target_uid, prompt_text) = match resolve_or_bind_im_sender(
        &hub,
        InboundMeta {
            channel: ImChannel::DingTalk,
            sender_id: &sender_id,
            is_group,
            robot_code: &robot_code,
        },
        &raw_text,
    )
    .await
    {
        RouteDecision::DirectReply(msg) => {
            if !session_webhook.is_empty() {
                let _ = send_dingtalk_session_webhook(&hub, &session_webhook, "Agenct 提示", &msg)
                    .await;
            }
            return;
        }
        RouteDecision::Ignore => return,
        RouteDecision::Routed { user_id, text } => (user_id, text),
    };

    if !session_webhook.is_empty() {
        hub.im_runtime
            .cache_dd_session_webhook(&target_uid, session_webhook.clone(), exp_sec);
    }

    let req_id = format!("dd:{dedup_id}");
    let reply = match run_turn(&hub, &target_uid, &prompt_text, Some(&req_id), None).await {
        Ok(val) => val["reply"].as_str().unwrap_or("好的，已处理。").to_owned(),
        Err(err) => format!("处理失败：{}", err.1),
    };

    let sent_via_hook = !session_webhook.is_empty()
        && send_dingtalk_session_webhook(&hub, &session_webhook, "Agenct 回复", &reply)
            .await
            .is_ok();
    if !sent_via_hook {
        let user_dd_cfg = load_im_config(&hub, &target_uid)
            .await
            .map(|c| c.dingtalk)
            .unwrap_or_else(|_| DingTalkBotConfig {
                bound_staff_id: sender_id,
                robot_code,
                ..Default::default()
            });
        let _ =
            send_dingtalk_proactive(&hub, &target_uid, &user_dd_cfg, "Agenct 回复", &reply).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{clean_im_text, parse_bind_command, sign_webhook_request};
    use serde_json::json;

    #[test]
    fn clean_text_and_parse_bind_handle_mentions_and_aliases() {
        assert_eq!(
            clean_im_text("  @Agenct   /bind a1b2c3 ", true),
            "/bind a1b2c3"
        );
        assert_eq!(clean_im_text("@Agenct @bob 请核查", true), "@bob 请核查");
        assert_eq!(clean_im_text("  hello  ", false), "hello");

        assert_eq!(parse_bind_command("/bind k7m4p9"), Some("K7M4P9".into()));
        assert_eq!(parse_bind_command("绑定 AB12CD"), Some("AB12CD".into()));
        assert_eq!(parse_bind_command("/bind short"), None);
        assert_eq!(parse_bind_command("绑定这个吗"), None);
    }

    #[test]
    fn webhook_signing_adds_expected_fields() {
        let (dd_url, dd_body) = sign_webhook_request(
            "dingtalk",
            "https://oapi.dingtalk.com/robot/send?access_token=abc",
            "SEC_test",
            json!({"msgtype": "markdown"}),
        );
        assert!(dd_url.contains("access_token=abc&timestamp="));
        assert!(dd_url.contains("&sign="));
        assert_eq!(dd_body["msgtype"], "markdown");

        let (fs_url, fs_body) = sign_webhook_request(
            "feishu",
            "https://open.feishu.cn/open-apis/bot/v2/hook/xyz",
            "feishu_sec",
            json!({"msg_type": "interactive"}),
        );
        assert_eq!(fs_url, "https://open.feishu.cn/open-apis/bot/v2/hook/xyz");
        assert!(fs_body["timestamp"].as_str().is_some());
        assert!(fs_body["sign"].as_str().is_some());
    }
}
