//! Authentication: optional GitLab OAuth 2.0 (PKCE S256) team login and/or a single-owner
//! local token login, both issuing SQLite-backed browser sessions.
use crate::{config::now_unix, db::record_audit, denied, invalid, Error, Hub, Result};
use axum::{
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use cookie::{Cookie, SameSite};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use uuid::Uuid;

const SESSION_COOKIE_TLS: &str = "__Host-agenct-session";
const LOGIN_COOKIE_TLS: &str = "__Host-agenct-login";
const SESSION_COOKIE_HTTP: &str = "agenct-local-session";
const LOGIN_COOKIE_HTTP: &str = "agenct-local-login";
const MAX_PENDING_LOGINS: usize = 512;

#[derive(Clone, Debug, Serialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    pub status: String,
}

struct PendingLogin {
    binding_hash: String,
    verifier: String,
    expires: Instant,
}

#[derive(Clone, Default)]
pub struct OAuthPendingStore {
    pending: Arc<Mutex<HashMap<String, PendingLogin>>>,
}

pub fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn session_cookie_name(local_http: bool) -> &'static str {
    if local_http {
        SESSION_COOKIE_HTTP
    } else {
        SESSION_COOKIE_TLS
    }
}

fn login_cookie_name(local_http: bool) -> &'static str {
    if local_http {
        LOGIN_COOKIE_HTTP
    } else {
        LOGIN_COOKIE_TLS
    }
}

fn build_cookie(
    local_http: bool,
    name: &'static str,
    value: String,
    max_age_secs: i64,
) -> HeaderValue {
    let mut builder = Cookie::build((name, value))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(cookie::time::Duration::seconds(max_age_secs));
    if !local_http {
        builder = builder.secure(true);
    }
    HeaderValue::from_str(&builder.build().to_string()).expect("ASCII cookie header")
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for header_val in headers.get_all(header::COOKIE) {
        let raw = header_val.to_str().ok()?;
        for parsed in Cookie::split_parse(raw) {
            let c = parsed.ok()?;
            if c.name() == name {
                if found.is_some() {
                    return None;
                }
                let val = c.value();
                if val.len() < 32
                    || val.len() > 128
                    || !val
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return None;
                }
                found = Some(val.to_owned());
            }
        }
    }
    found
}

impl Hub {
    pub async fn session_from_headers(
        &self,
        headers: &HeaderMap,
    ) -> Result<Option<(User, String)>> {
        let Some(raw) = read_cookie(headers, session_cookie_name(self.config.local_http)) else {
            return Ok(None);
        };
        let hash = sha256_hex(&raw);
        let now = now_unix();
        self.db
            .read(move |conn| {
                let row = conn
                    .query_row(
                        "SELECT u.id, u.username, u.display_name, u.is_admin, u.status, s.csrf_token
                         FROM sessions s
                         JOIN users u ON u.id = s.user_id
                         WHERE s.session_hash = ?1 AND s.expires_at > ?2",
                        params![hash, now],
                        |r| {
                            Ok((
                                User {
                                    id: r.get(0)?,
                                    username: r.get(1)?,
                                    display_name: r.get(2)?,
                                    is_admin: r.get::<_, i64>(3)? != 0,
                                    status: r.get(4)?,
                                },
                                r.get::<_, String>(5)?,
                            ))
                        },
                    )
                    .optional()?;
                Ok(row)
            })
            .await
    }

    pub async fn require_user(&self, headers: &HeaderMap) -> Result<User> {
        let (user, _csrf) = self
            .session_from_headers(headers)
            .await?
            .ok_or(Error(StatusCode::UNAUTHORIZED, "authentication required"))?;
        if user.status != "active" {
            return Err(Error(StatusCode::UNAUTHORIZED, "account disabled"));
        }
        Ok(user)
    }

    /// Creates a browser session for an existing or new user; returns `(session_cookie, csrf_token)`.
    pub async fn issue_session(&self, user_id: String, ttl_secs: i64) -> Result<(String, String)> {
        let token = random_token();
        let csrf = random_token();
        let hash = sha256_hex(&token);
        let expires = now_unix() + ttl_secs;
        let csrf_clone = csrf.clone();
        self.db
            .write(move |conn| {
                conn.execute("DELETE FROM sessions WHERE expires_at <= ?1", [now_unix()])?;
                conn.execute(
                    "INSERT INTO sessions(session_hash, user_id, csrf_token, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![hash, user_id, csrf_clone, expires],
                )?;
                Ok(())
            })
            .await?;
        Ok((token, csrf))
    }
}

/// Security middleware: Host pinning, Origin + CSRF verification on mutations, CSP & security headers.
pub async fn guard(
    State(hub): State<Hub>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let headers = request.headers();

    // Enforce Host header pinning when Host is provided (standard HTTP/1.1).
    let host_values: Vec<&str> = headers
        .get_all(header::HOST)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    if host_values.len() == 1 && host_values[0] != hub.config.authority() {
        return Error(StatusCode::UNAUTHORIZED, "unexpected host header").into_response();
    }
    if host_values.len() > 1 {
        return Error(StatusCode::BAD_REQUEST, "multiple host headers").into_response();
    }

    let is_mutation = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);
    let is_external_machine = path.starts_with("/node/v1/") || path.starts_with("/webhooks/");

    if is_mutation && !is_external_machine {
        let origin_ok = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|o| o == hub.config.public_origin);
        if !origin_ok {
            return Error(StatusCode::FORBIDDEN, "origin verification failed").into_response();
        }
    }
    // The local login endpoint has no session yet: Origin pinning above prevents login CSRF.
    if is_mutation && !is_external_machine && path != "/auth/local" {
        let provided_csrf = headers
            .get("x-csrf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        match hub.session_from_headers(headers).await {
            Ok(Some((_user, expected_csrf)))
                if crate::config::constant_time_eq(
                    provided_csrf.as_bytes(),
                    expected_csrf.as_bytes(),
                ) => {}
            _ => {
                return Error(StatusCode::FORBIDDEN, "CSRF verification failed").into_response();
            }
        }
    }

    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    response
}

pub async fn session(
    State(hub): State<Hub>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>> {
    match hub.session_from_headers(&headers).await? {
        Some((user, csrf)) if user.status == "active" => Ok(Json(json!({
            "authenticated": true,
            "user": user,
            "csrf_token": csrf,
            "llm": hub.llm.label(),
        }))),
        Some((user, _)) => Ok(Json(json!({
            "authenticated": false,
            "reason": "disabled",
            "user": user.username,
        }))),
        None => Ok(Json(json!({
            "authenticated": false,
            "login": {
                "gitlab": hub.config.gitlab.is_some(),
                "local": hub.config.local_login,
            },
            "gitlab_url": hub.config.gitlab.as_ref().map(|g| g.base_url.clone()),
        }))),
    }
}

pub async fn login(State(hub): State<Hub>) -> Result<Response> {
    let gl = hub.config.gitlab.as_ref().ok_or_else(denied)?;
    let verifier = random_token();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_token();
    let binding = random_token();
    let binding_hash = sha256_hex(&binding);

    {
        let mut map = hub
            .oauth_pending
            .pending
            .lock()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "oauth store unavailable"))?;
        let now = Instant::now();
        map.retain(|_, v| v.expires > now);
        if map.len() >= MAX_PENDING_LOGINS {
            return Err(Error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many pending logins",
            ));
        }
        map.insert(
            sha256_hex(&state),
            PendingLogin {
                binding_hash,
                verifier,
                expires: now + Duration::from_secs(300),
            },
        );
    }

    let redirect_uri = format!("{}{}", hub.config.public_origin, gl.callback_path);
    let mut auth_url =
        reqwest::Url::parse(&format!("{}/oauth/authorize", gl.base_url)).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "invalid gitlab authorize URL",
            )
        })?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &gl.client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "read_user")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    let mut resp = Redirect::to(auth_url.as_str()).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_cookie(
            hub.config.local_http,
            login_cookie_name(hub.config.local_http),
            binding,
            300,
        ),
    );
    Ok(resp)
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: String,
    state: String,
}

pub async fn callback(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> Response {
    match callback_inner(&hub, &headers, q).await {
        Ok(resp) => resp,
        Err(Error(_, stage)) => {
            eprintln!(
                "{}",
                json!({"event": "gitlab_login_failed", "stage": stage})
            );
            Redirect::to(&format!("/?login_error={stage}")).into_response()
        }
    }
}

async fn callback_inner(hub: &Hub, headers: &HeaderMap, q: CallbackQuery) -> Result<Response> {
    let gl = hub
        .config
        .gitlab
        .as_ref()
        .ok_or(Error(StatusCode::NOT_FOUND, "disabled"))?;
    if q.code.is_empty() || q.code.len() > 2048 || q.state.is_empty() || q.state.len() > 256 {
        return Err(Error(StatusCode::BAD_REQUEST, "invalid"));
    }
    let binding = read_cookie(headers, login_cookie_name(hub.config.local_http))
        .ok_or(Error(StatusCode::UNAUTHORIZED, "expired"))?;
    let pending = {
        let mut map = hub
            .oauth_pending
            .pending
            .lock()
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "failed"))?;
        let key = sha256_hex(&q.state);
        let entry = map
            .remove(&key)
            .ok_or(Error(StatusCode::UNAUTHORIZED, "expired"))?;
        if entry.expires <= Instant::now()
            || !crate::config::constant_time_eq(
                entry.binding_hash.as_bytes(),
                sha256_hex(&binding).as_bytes(),
            )
        {
            return Err(Error(StatusCode::UNAUTHORIZED, "expired"));
        }
        entry
    };

    let redirect_uri = format!("{}{}", hub.config.public_origin, gl.callback_path);
    let token_resp = hub
        .http
        .post(format!("{}/oauth/token", gl.base_url))
        .form(&[
            ("client_id", gl.client_id.as_str()),
            ("client_secret", gl.client_secret.as_str()),
            ("code", q.code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri.as_str()),
            ("code_verifier", pending.verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|_| Error(StatusCode::BAD_GATEWAY, "provider"))?;
    if !token_resp.status().is_success() {
        return Err(Error(StatusCode::BAD_GATEWAY, "provider"));
    }
    let token_json: serde_json::Value = token_resp
        .json()
        .await
        .map_err(|_| Error(StatusCode::BAD_GATEWAY, "provider"))?;
    let access_token = token_json["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(Error(StatusCode::BAD_GATEWAY, "provider"))?;

    let user_resp = hub
        .http
        .get(format!("{}/api/v4/user", gl.base_url))
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|_| Error(StatusCode::BAD_GATEWAY, "userinfo"))?;
    if !user_resp.status().is_success() {
        return Err(Error(StatusCode::BAD_GATEWAY, "userinfo"));
    }
    let profile: serde_json::Value = user_resp
        .json()
        .await
        .map_err(|_| Error(StatusCode::BAD_GATEWAY, "userinfo"))?;

    let gitlab_id = profile["id"]
        .as_i64()
        .filter(|id| *id > 0)
        .ok_or(Error(StatusCode::UNAUTHORIZED, "account"))?;
    if profile["state"].as_str() != Some("active") {
        return Err(Error(StatusCode::UNAUTHORIZED, "account"));
    }
    let username = profile["username"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .unwrap_or("user")
        .to_ascii_lowercase();
    let display_name = profile["name"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(&username)
        .chars()
        .take(80)
        .collect::<String>();

    let user_id = format!("gl-{gitlab_id}");
    let is_configured_admin = hub.config.admin_usernames.contains(&username);
    let uid_clone = user_id.clone();

    let active = hub
        .db
        .write(move |conn| {
            let existing_count: i64 =
                conn.query_row("SELECT count(*) FROM users", [], |r| r.get(0))?;
            let is_admin = if is_configured_admin || existing_count == 0 {
                1
            } else {
                0
            };
            conn.execute(
                "INSERT INTO users(id, gitlab_id, username, display_name, is_admin, status, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    username = excluded.username,
                    display_name = excluded.display_name,
                    is_admin = CASE WHEN excluded.is_admin = 1 THEN 1 ELSE users.is_admin END",
                params![uid_clone, gitlab_id, username, display_name, is_admin, now_unix()],
            )?;
            let status: String = conn.query_row(
                "SELECT status FROM users WHERE id = ?1",
                [&uid_clone],
                |r| r.get(0),
            )?;
            Ok(status == "active")
        })
        .await?;

    if !active {
        return Err(Error(StatusCode::UNAUTHORIZED, "account"));
    }

    let (session_token, _csrf) = hub
        .issue_session(user_id, hub.config.session_ttl_seconds)
        .await?;
    let mut resp = Redirect::to("/").into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_cookie(
            hub.config.local_http,
            login_cookie_name(hub.config.local_http),
            String::new(),
            0,
        ),
    );
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_cookie(
            hub.config.local_http,
            session_cookie_name(hub.config.local_http),
            session_token,
            hub.config.session_ttl_seconds,
        ),
    );
    Ok(resp)
}

pub async fn logout(State(hub): State<Hub>, headers: HeaderMap) -> Result<Response> {
    if let Some(raw) = read_cookie(&headers, session_cookie_name(hub.config.local_http)) {
        let hash = sha256_hex(&raw);
        hub.db
            .write(move |conn| {
                conn.execute("DELETE FROM sessions WHERE session_hash = ?1", [hash])?;
                Ok(())
            })
            .await?;
    }
    let mut resp = Json(json!({"ok": true})).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_cookie(
            hub.config.local_http,
            session_cookie_name(hub.config.local_http),
            String::new(),
            0,
        ),
    );
    Ok(resp)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateMemberStatus {
    status: String,
}

pub async fn list_members(State(hub): State<Hub>, headers: HeaderMap) -> Result<Json<Vec<User>>> {
    let caller = hub.require_user(&headers).await?;
    let include_disabled = caller.is_admin;
    let users = hub
        .db
        .read(move |conn| {
            let sql = if include_disabled {
                "SELECT id, username, display_name, is_admin, status FROM users ORDER BY username ASC"
            } else {
                "SELECT id, username, display_name, is_admin, status FROM users WHERE status='active' ORDER BY username ASC"
            };
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| {
                Ok(User {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    display_name: r.get(2)?,
                    is_admin: r.get::<_, i64>(3)? != 0,
                    status: r.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    Ok(Json(users))
}

pub async fn set_member_status(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Path(target_id): Path<String>,
    Json(input): Json<UpdateMemberStatus>,
) -> Result<Json<serde_json::Value>> {
    let caller = hub.require_user(&headers).await?;
    if !caller.is_admin {
        return Err(denied());
    }
    if target_id == caller.id {
        return Err(invalid("cannot disable your own admin account"));
    }
    if !matches!(input.status.as_str(), "active" | "disabled") {
        return Err(invalid("status must be active or disabled"));
    }
    let next_status = input.status;
    let caller_id = caller.id;
    let target_clone = target_id.clone();
    hub.db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE users SET status = ?2 WHERE id = ?1",
                params![target_clone, next_status],
            )?;
            if changed == 0 {
                return Err(denied());
            }
            if next_status == "disabled" {
                conn.execute("DELETE FROM sessions WHERE user_id = ?1", [&target_clone])?;
                conn.execute(
                    "UPDATE node_jobs SET status='failed', updated_at=?2
                     WHERE user_id = ?1 AND status IN ('queued','running')",
                    params![target_clone, now_unix()],
                )?;
            }
            record_audit(
                conn,
                &caller_id,
                "member_status",
                "ok",
                &format!("Member {target_clone} -> {next_status}"),
                "Updated by admin",
            )?;
            Ok(())
        })
        .await?;
    Ok(Json(json!({"ok": true, "id": target_id})))
}

// ---------------------------------------------------------------------------
// Local owner login (no external identity provider)
// ---------------------------------------------------------------------------

/// User id of the single local owner account.
pub const LOCAL_OWNER_ID: &str = "local-owner";
const LOCAL_TOKEN_FILE: &str = "login-token";

/// Serializes local login attempts; failed attempts hold it during a penalty delay, capping
/// brute force at roughly one guess per second regardless of concurrency.
static LOCAL_LOGIN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn write_token_file(cfg: &crate::config::Config, token: &str) -> Option<std::path::PathBuf> {
    if cfg.is_memory_db() {
        return None;
    }
    let path = cfg.data_dir.join(LOCAL_TOKEN_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(&path).ok()?;
    f.write_all(format!("{token}\n").as_bytes()).ok()?;
    Some(path)
}

fn store_local_token(conn: &rusqlite::Connection, token: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO local_auth(id, token_hash, created_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET token_hash = excluded.token_hash, created_at = excluded.created_at",
        params![sha256_hex(token), now_unix()],
    )?;
    Ok(())
}

/// Prepares local login at startup. Returns `(token, saved_path)` when a new token was
/// generated and must be shown to the operator once.
pub fn init_local_login(
    conn: &rusqlite::Connection,
    cfg: &crate::config::Config,
) -> Result<Option<(String, Option<std::path::PathBuf>)>> {
    if !cfg.local_login {
        return Ok(None);
    }
    if let Some(ref fixed) = cfg.local_login_token {
        store_local_token(conn, fixed)?;
        return Ok(None);
    }
    let exists: bool = conn.query_row("SELECT count(*) > 0 FROM local_auth", [], |r| r.get(0))?;
    if exists {
        return Ok(None);
    }
    rotate_local_token(conn, cfg).map(Some)
}

/// Generates and stores a fresh local login token (invalidates the previous one).
pub fn rotate_local_token(
    conn: &rusqlite::Connection,
    cfg: &crate::config::Config,
) -> Result<(String, Option<std::path::PathBuf>)> {
    let token = format!("agl_{}", random_token());
    store_local_token(conn, &token)?;
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", [LOCAL_OWNER_ID])?;
    Ok((token.clone(), write_token_file(cfg, &token)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalLoginInput {
    token: String,
}

pub async fn local_login(
    State(hub): State<Hub>,
    Json(input): Json<LocalLoginInput>,
) -> Result<Response> {
    if !hub.config.local_login {
        return Err(denied());
    }
    let _gate = LOCAL_LOGIN_GATE.lock().await;
    let token = input.token.trim().to_owned();
    let stored: Option<String> = hub
        .db
        .read(|conn| {
            Ok(conn
                .query_row("SELECT token_hash FROM local_auth WHERE id = 1", [], |r| {
                    r.get(0)
                })
                .optional()?)
        })
        .await?;
    let valid = !token.is_empty()
        && token.len() <= 256
        && stored.is_some_and(|h| {
            crate::config::constant_time_eq(h.as_bytes(), sha256_hex(&token).as_bytes())
        });
    if !valid {
        tokio::time::sleep(Duration::from_millis(1000)).await;
        return Err(Error(StatusCode::UNAUTHORIZED, "invalid login token"));
    }

    let username = std::env::var("AGENCT_LOCAL_USERNAME")
        .ok()
        .map(|u| u.trim().to_ascii_lowercase())
        .filter(|u| !u.is_empty() && u.len() <= 64)
        .unwrap_or_else(|| "owner".into());
    let active = hub
        .db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO users(id, username, display_name, is_admin, status, created_at)
                 VALUES (?1, ?2, ?3, 1, 'active', ?4)
                 ON CONFLICT(id) DO NOTHING",
                params![LOCAL_OWNER_ID, username, "Owner", now_unix()],
            )?;
            let status: String = conn.query_row(
                "SELECT status FROM users WHERE id = ?1",
                [LOCAL_OWNER_ID],
                |r| r.get(0),
            )?;
            record_audit(conn, LOCAL_OWNER_ID, "login", "ok", "Local token login", "")?;
            Ok(status == "active")
        })
        .await?;
    if !active {
        return Err(Error(StatusCode::UNAUTHORIZED, "account disabled"));
    }

    let (session_token, csrf) = hub
        .issue_session(LOCAL_OWNER_ID.to_owned(), hub.config.session_ttl_seconds)
        .await?;
    let mut resp = Json(json!({"ok": true, "csrf_token": csrf})).into_response();
    resp.headers_mut().append(
        header::SET_COOKIE,
        build_cookie(
            hub.config.local_http,
            session_cookie_name(hub.config.local_http),
            session_token,
            hub.config.session_ttl_seconds,
        ),
    );
    Ok(resp)
}
