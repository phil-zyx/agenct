//! Opt-in OIDC or GitLab OAuth browser sessions. Explicit bindings; no email matching.
use crate::{Error, Hub};
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use cookie::{Cookie, SameSite};
use openidconnect::{
    core::{CoreAuthenticationFlow, CoreClient, CoreJsonWebKeySet, CoreJwsSigningAlgorithm},
    AccessTokenHash, AuthUrl, AuthorizationCode, ClientId, CsrfToken, EndpointNotSet, EndpointSet,
    IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl,
    TokenResponse, TokenUrl,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
const SESSION_COOKIE: &str = "__Host-agenct-session";
const LOGIN_COOKIE: &str = "__Host-agenct-login";
const MAX_ENTRIES: usize = 1024;
type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
    EndpointNotSet,
>;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub gitlab_oauth: bool,
    #[serde(default)]
    pub shared_gitlab_login: Option<String>,
    #[serde(default)]
    pub shared_gitlab_callback: Option<String>,
    #[serde(default)]
    pub allow_loopback_http: bool,
    #[serde(default)]
    pub allow_private_http: bool,
    #[serde(default)]
    pub callback_path: Option<String>,
    #[serde(default)]
    pub gitlab_auto_signup: bool,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub client_id: String,
    pub public_origin: String,
    pub subjects: HashMap<String, String>,
    pub jwks: serde_json::Value,
}
struct Pending {
    binding: String,
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    expires: Instant,
}
#[derive(Clone)]
struct Session {
    binding: String,
    user: String,
    csrf: String,
    expires: Instant,
}
#[derive(Default)]
struct Store {
    bindings: HashMap<String, Instant>,
    pending: HashMap<String, Pending>,
    sessions: HashMap<String, Session>,
}
#[derive(Clone)]
pub struct BrowserAuth {
    gitlab_oauth: bool,
    shared_gitlab_login: Option<String>,
    local_http: bool,
    pub(crate) gitlab_auto_signup: bool,
    client: Client,
    http: reqwest::Client,
    origin: String,
    issuer: String,
    authority: String,
    client_id: String,
    subjects: HashMap<String, String>,
    store: Arc<Mutex<Store>>,
}
fn bad() -> Error {
    Error(StatusCode::UNAUTHORIZED, "browser authentication failed")
}
fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}
fn random() -> String {
    CsrfToken::new_random_len(32).secret().to_owned()
}
fn cookie(name: &'static str, value: String, seconds: i64) -> HeaderValue {
    HeaderValue::from_str(
        &Cookie::build((name, value))
            .path("/")
            .secure(true)
            .http_only(true)
            .same_site(SameSite::Lax)
            .max_age(cookie::time::Duration::seconds(seconds))
            .build()
            .to_string(),
    )
    .expect("generated cookie is ASCII")
}
fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        for parsed in Cookie::split_parse(value.to_str().ok()?) {
            let parsed = parsed.ok()?;
            if parsed.name() == name {
                if found.is_some() {
                    return None;
                }
                let value = parsed.value();
                if value.len() > 128
                    || value.len() < 32
                    || !value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return None;
                }
                found = Some(value.to_owned());
            }
        }
    }
    found
}
fn https(value: &str) -> Result<reqwest::Url, &'static str> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid browser OIDC URL")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("browser OIDC endpoints require clean HTTPS URLs");
    }
    Ok(url)
}
fn clean_url(value: &str) -> Result<reqwest::Url, &'static str> {
    let url = reqwest::Url::parse(value).map_err(|_| "invalid login URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("invalid login URL");
    }
    Ok(url)
}
fn loopback_http(value: &str) -> bool {
    clean_url(value).is_ok_and(|url| {
        url.scheme() == "http"
            && matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
    })
}
pub(crate) fn private_http(value: &str) -> bool {
    loopback_http(value)
        || clean_url(value).is_ok_and(|url| {
            url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| {
                        let octets = ip.octets();
                        ip.is_private() || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                    })
                })
        })
}
impl BrowserAuth {
    fn cookie_name(&self, name: &'static str) -> &'static str {
        if self.local_http {
            if name == SESSION_COOKIE {
                "agenct-local-session"
            } else {
                "agenct-local-login"
            }
        } else {
            name
        }
    }
    fn cookie(&self, name: &'static str, value: String, seconds: i64) -> HeaderValue {
        if !self.local_http {
            return cookie(name, value, seconds);
        }
        HeaderValue::from_str(
            &Cookie::build((self.cookie_name(name), value))
                .path("/")
                .http_only(true)
                .same_site(SameSite::Lax)
                .max_age(cookie::time::Duration::seconds(seconds))
                .build()
                .to_string(),
        )
        .expect("generated cookie is ASCII")
    }
    pub fn new(config: Config, known: &HashSet<String>) -> Result<Self, &'static str> {
        // Identity-provider calls (token exchange, userinfo) ignore ambient proxy
        // variables. Where the provider is only reachable through a proxy, the
        // operator names it explicitly; HTTPS still tunnels end to end (CONNECT),
        // so the proxy never sees codes or tokens. Other Hub traffic is unaffected.
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15));
        if let Some(proxy) = std::env::var("AGENCT_IDENTITY_PROXY")
            .ok()
            .filter(|value| !value.trim().is_empty())
        {
            let url =
                reqwest::Url::parse(proxy.trim()).map_err(|_| "invalid AGENCT_IDENTITY_PROXY")?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err("AGENCT_IDENTITY_PROXY must be an http(s) proxy URL");
            }
            builder = builder.proxy(
                reqwest::Proxy::https(url.as_str()).map_err(|_| "invalid AGENCT_IDENTITY_PROXY")?,
            );
        }
        let http = builder.build().map_err(|_| "TLS client unavailable")?;
        let mut auth = Self::with_http(config, known, http)?;
        if let Some(path) = std::env::var_os("AGENCT_OIDC_CLIENT_SECRET_FILE") {
            let secret =
                std::fs::read_to_string(path).map_err(|_| "cannot read OIDC client secret file")?;
            let secret = secret.trim();
            if secret.is_empty() || secret.len() > 4096 {
                return Err("invalid OIDC client secret");
            }
            auth.client = auth
                .client
                .set_client_secret(openidconnect::ClientSecret::new(secret.into()))
                .set_auth_type(openidconnect::AuthType::BasicAuth);
        }
        Ok(auth)
    }
    fn with_http(
        config: Config,
        known: &HashSet<String>,
        http: reqwest::Client,
    ) -> Result<Self, &'static str> {
        let issuer = https(&config.issuer)?;
        let authorization = https(&config.authorization_endpoint)?;
        let token = https(&config.token_endpoint)?;
        let local_http = (config.allow_loopback_http && loopback_http(&config.public_origin))
            || (config.allow_private_http && private_http(&config.public_origin));
        let callback_path = config.callback_path.as_deref().unwrap_or("/auth/callback");
        if !matches!(
            callback_path,
            "/auth/callback" | "/identity/v1/auth/oidc/gitlab/callback"
        ) {
            return Err("unsupported browser callback path");
        }
        let origin = if local_http {
            reqwest::Url::parse(&config.public_origin).map_err(|_| "invalid local origin")?
        } else {
            https(&config.public_origin)?
        };
        match (&config.shared_gitlab_login, &config.shared_gitlab_callback) {
            (Some(login), Some(callback)) if config.gitlab_oauth => {
                clean_url(login)?;
                if !login.starts_with("https://") && !loopback_http(login) {
                    return Err("shared login requires HTTPS or loopback");
                }
                clean_url(callback)?;
            }
            (None, None) => {}
            _ => return Err("shared GitLab login requires a matching callback"),
        }
        if authorization.origin() != issuer.origin()
            || token.origin() != issuer.origin()
            || origin.path() != "/"
            || config.public_origin != origin.origin().ascii_serialization()
            || config.client_id.is_empty()
            || config.client_id.len() > 256
            || (config.subjects.is_empty() && !config.gitlab_auto_signup)
            || (config.gitlab_auto_signup && (!config.gitlab_oauth || !config.subjects.is_empty()))
            || config
                .subjects
                .iter()
                .any(|(sub, user)| sub.is_empty() || sub.len() > 255 || !known.contains(user))
        {
            return Err("invalid browser OIDC trust policy");
        }
        let jwks: CoreJsonWebKeySet = if config.gitlab_oauth {
            serde_json::from_value(serde_json::json!({"keys":[]}))
                .map_err(|_| "invalid OAuth key set")?
        } else {
            // Reuse the narrow public-key schema and key policy; no token-provided key URLs.
            let keys: crate::oidc::KeySet = serde_json::from_value(config.jwks.clone())
                .map_err(|_| "invalid pinned public keys")?;
            crate::oidc::Validator::new(
                crate::oidc::Config {
                    issuer: config.issuer.clone(),
                    audience: config.client_id.clone(),
                    required_scope: "unused".into(),
                    subjects: config.subjects.clone(),
                    jwks: keys,
                },
                known,
            )?;
            let parsed: CoreJsonWebKeySet = serde_json::from_value(config.jwks.clone())
                .map_err(|_| "invalid pinned public keys")?;
            parsed
        };
        let client = CoreClient::new(
            ClientId::new(config.client_id.clone()),
            IssuerUrl::new(config.issuer.clone()).map_err(|_| "invalid issuer")?,
            jwks,
        )
        .set_auth_uri(
            AuthUrl::new(config.authorization_endpoint).map_err(|_| "invalid authorization URL")?,
        )
        .set_token_uri(TokenUrl::new(config.token_endpoint).map_err(|_| "invalid token URL")?)
        .set_redirect_uri(
            RedirectUrl::new(
                config
                    .shared_gitlab_callback
                    .clone()
                    .unwrap_or_else(|| format!("{}{callback_path}", config.public_origin)),
            )
            .map_err(|_| "invalid callback")?,
        );
        let client = if config.gitlab_oauth {
            client.disable_openid_scope()
        } else {
            client
        };
        let authority = config
            .public_origin
            .strip_prefix(if local_http { "http://" } else { "https://" })
            .ok_or("invalid origin")?
            .to_owned();
        Ok(Self {
            gitlab_oauth: config.gitlab_oauth,
            shared_gitlab_login: config.shared_gitlab_login,
            local_http,
            gitlab_auto_signup: config.gitlab_auto_signup,
            client,
            http,
            origin: config.public_origin,
            issuer: config.issuer,
            authority,
            client_id: config.client_id,
            subjects: config.subjects,
            store: Arc::new(Mutex::new(Store::default())),
        })
    }
    pub(crate) fn has_user(&self, user: &str) -> bool {
        self.subjects.values().any(|id| id == user)
            || (self.gitlab_auto_signup
                && user
                    .strip_prefix("gitlab-")
                    .is_some_and(|id| id.parse::<u64>().is_ok()))
    }
    pub(crate) fn revoke_user(&self, user: &str) {
        if let Ok(mut store) = self.store.lock() {
            store.sessions.retain(|_, session| session.user != user);
        }
    }
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let value = read_cookie(headers, self.cookie_name(SESSION_COOKIE))?;
        let mut store = self.store.lock().ok()?;
        store
            .sessions
            .retain(|_, session| session.expires > Instant::now());
        store.sessions.get(&hash(&value)).cloned()
    }
    pub(crate) fn identity(&self, headers: &HeaderMap) -> Result<String, Error> {
        if headers.contains_key(header::AUTHORIZATION) {
            return Err(bad());
        }
        self.session(headers).map(|s| s.user).ok_or_else(bad)
    }
    fn page_session_matches(&self, headers: &HeaderMap) -> bool {
        let Some(session) = self.session(headers) else {
            return false;
        };
        headers.get_all("x-csrf-token").iter().count() == 1
            && headers
                .get("x-csrf-token")
                .and_then(|x| x.to_str().ok())
                .map(hash)
                == Some(hash(&session.csrf))
    }
    fn mutation_allowed(&self, headers: &HeaderMap) -> bool {
        let Some(session) = self.session(headers) else {
            return false;
        };
        headers.get_all(header::ORIGIN).iter().count() == 1
            && headers.get(header::ORIGIN).and_then(|x| x.to_str().ok())
                == Some(self.origin.as_str())
            && headers.get_all("x-csrf-token").iter().count() == 1
            && headers
                .get("x-csrf-token")
                .and_then(|x| x.to_str().ok())
                .map(hash)
                == Some(hash(&session.csrf))
    }
}
pub(crate) async fn guard(
    State(h): State<Hub>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(auth) = &h.browser_auth {
        if request.headers().get_all(header::HOST).iter().count() != 1
            || request
                .headers()
                .get(header::HOST)
                .and_then(|x| x.to_str().ok())
                != Some(auth.authority.as_str())
        {
            return bad().into_response();
        }
        if request.uri().path().starts_with("/api/")
            && !auth.page_session_matches(request.headers())
        {
            return bad().into_response();
        }
        if !matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
        ) && !auth.mutation_allowed(request.headers())
        {
            return Error(
                StatusCode::FORBIDDEN,
                "origin and CSRF verification required",
            )
            .into_response();
        }
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"));
    response
}
pub(crate) async fn session(State(h): State<Hub>, headers: HeaderMap) -> Json<serde_json::Value> {
    let Some(ref auth) = h.browser_auth else {
        return Json(serde_json::json!({"enabled":false}));
    };
    match auth.session(&headers) {
        Some(s) => Json(
            serde_json::json!({"enabled":true,"authenticated":true,"user":s.user,"csrf_token":s.csrf,"member_status":h.member(&s.user).ok().map(|m|m.status)}),
        ),
        None => Json(serde_json::json!({"enabled":true,"authenticated":false})),
    }
}
pub(crate) async fn login(State(h): State<Hub>, headers: HeaderMap) -> Result<Response, Error> {
    let auth = h.browser_auth.ok_or_else(bad)?;
    if auth.session(&headers).is_some() {
        return Err(Error(
            StatusCode::CONFLICT,
            "log out before starting a new login",
        ));
    }
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = auth
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            || CsrfToken::new_random_len(32),
            Nonce::new_random,
        )
        .add_scope(openidconnect::Scope::new(
            if auth.gitlab_oauth {
                "read_user"
            } else {
                "profile"
            }
            .into(),
        ))
        .set_pkce_challenge(challenge)
        .url();
    let url = if let Some(endpoint) = &auth.shared_gitlab_login {
        let mut shared = reqwest::Url::parse(endpoint).map_err(|_| bad())?;
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        shared
            .query_pairs_mut()
            .append_pair("origin", &auth.origin)
            .append_pair("response_mode", "code_handoff")
            .append_pair("state", state.secret())
            .append_pair("code_challenge", &params["code_challenge"])
            .append_pair("code_challenge_method", "S256");
        shared
    } else {
        url
    };
    let binding = random();
    let mut store = auth.store.lock().map_err(|_| bad())?;
    store
        .pending
        .retain(|_, pending| pending.expires > Instant::now());
    store
        .bindings
        .retain(|_, expires| *expires > Instant::now());
    if store.bindings.len() >= MAX_ENTRIES {
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "login capacity reached",
        ));
    }
    if let Some(old) = read_cookie(&headers, auth.cookie_name(LOGIN_COOKIE)) {
        store
            .pending
            .retain(|_, pending| pending.binding != hash(&old));
        store.bindings.remove(&hash(&old));
    }
    store
        .bindings
        .insert(hash(&binding), Instant::now() + Duration::from_secs(300));
    store.pending.insert(
        hash(state.secret()),
        Pending {
            binding: hash(&binding),
            verifier,
            nonce,
            expires: Instant::now() + Duration::from_secs(300),
        },
    );
    let mut response = Redirect::to(url.as_str()).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth.cookie(LOGIN_COOKIE, binding, 300));
    Ok(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Callback {
    code: String,
    state: String,
    iss: Option<String>,
}
/// A failed login returns the browser to the app with a coarse reason it can
/// explain, and leaves a content-free operator log line naming the stage.
/// Never includes codes, tokens, provider bodies or user details.
fn login_failed(stage: &'static str) -> Error {
    eprintln!(
        "{}",
        serde_json::json!({"event":"login_callback_failed","stage":stage})
    );
    Error(StatusCode::UNAUTHORIZED, stage)
}
pub(crate) async fn callback(
    State(h): State<Hub>,
    headers: HeaderMap,
    query: Query<Callback>,
) -> Response {
    match callback_inner(h, headers, query).await {
        Ok(response) => response,
        Err(Error(_, stage)) => {
            let reason = match stage {
                "expired" => "expired",
                "provider" | "userinfo" => "provider",
                "account" => "account",
                _ => "failed",
            };
            Redirect::to(&format!("/?login_error={reason}")).into_response()
        }
    }
}
async fn callback_inner(
    h: Hub,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response, Error> {
    let auth = h
        .browser_auth
        .as_ref()
        .ok_or_else(|| login_failed("disabled"))?;
    if query.code.len() > 4096 || query.state.len() > 256 {
        return Err(login_failed("invalid"));
    }
    // Missing or unknown state: the login began in another browser, more than
    // five minutes ago, or before the Hub restarted (pending logins live in memory).
    let binding = read_cookie(&headers, auth.cookie_name(LOGIN_COOKIE))
        .ok_or_else(|| login_failed("expired"))?;
    let pending = {
        let mut store = auth.store.lock().map_err(|_| login_failed("failed"))?;
        let key = hash(&query.state);
        let pending = store
            .pending
            .get(&key)
            .ok_or_else(|| login_failed("expired"))?;
        if pending.binding != hash(&binding) || pending.expires <= Instant::now() {
            return Err(login_failed("expired"));
        }
        store
            .pending
            .remove(&key)
            .ok_or_else(|| login_failed("expired"))?
    };
    if query
        .iss
        .as_ref()
        .is_some_and(|iss| iss != auth.issuer.as_str())
    {
        return Err(login_failed("invalid"));
    }
    // Bound the response while streaming; never print provider errors or tokens.
    let http = |request: openidconnect::HttpRequest| {
        let client = auth.http.clone();
        async move {
            let mut response = client
                .execute(request.try_into().map_err(std::io::Error::other)?)
                .await
                .map_err(std::io::Error::other)?;
            let mut builder = axum::http::Response::builder().status(response.status());
            for (name, value) in response.headers() {
                builder = builder.header(name, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(std::io::Error::other)? {
                if body.len() + chunk.len() > 131072 {
                    return Err(std::io::Error::other("token response exceeds limit"));
                }
                body.extend_from_slice(&chunk);
            }
            builder.body(body).map_err(std::io::Error::other)
        }
    };
    let token = auth
        .client
        .exchange_code(AuthorizationCode::new(query.code))
        .set_pkce_verifier(pending.verifier)
        .request_async(&http)
        .await
        .map_err(|_| login_failed("provider"))?;
    let (user, display_name, lifetime) = if auth.gitlab_oauth {
        let mut response = auth
            .http
            .get(format!("{}/api/v4/user", auth.issuer))
            .bearer_auth(token.access_token().secret())
            .send()
            .await
            .map_err(|_| login_failed("userinfo"))?;
        if !response.status().is_success() {
            return Err(login_failed("userinfo"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| bad())? {
            if bytes.len() + chunk.len() > 131072 {
                return Err(bad());
            }
            bytes.extend_from_slice(&chunk);
        }
        let profile: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| bad())?;
        let subject = profile["id"]
            .as_u64()
            .filter(|id| *id > 0)
            .ok_or_else(bad)?
            .to_string();
        if profile["state"]
            .as_str()
            .is_some_and(|state| state != "active")
        {
            return Err(login_failed("account"));
        }
        let user = if auth.gitlab_auto_signup {
            if profile["state"].as_str() != Some("active") {
                return Err(login_failed("account"));
            }
            format!("gitlab-{subject}")
        } else {
            auth.subjects
                .get(&subject)
                .cloned()
                .ok_or_else(|| login_failed("account"))?
        };
        let lifetime = token
            .expires_in()
            .map(|value| value.as_secs().min(900))
            .unwrap_or(900) as i64;
        if lifetime <= 0 {
            return Err(bad());
        }
        (user, profile["name"].as_str().map(str::to_owned), lifetime)
    } else {
        let id = token.id_token().ok_or_else(bad)?;
        let verifier = auth
            .client
            .id_token_verifier()
            .set_allowed_algs([CoreJwsSigningAlgorithm::RsaSsaPkcs1V15Sha256]);
        let claims = id.claims(&verifier, &pending.nonce).map_err(|_| bad())?;
        if claims
            .authorized_party()
            .is_some_and(|party| party.as_str() != auth.client_id)
        {
            return Err(bad());
        }
        if let Some(expected) = claims.access_token_hash() {
            let actual = AccessTokenHash::from_token(
                token.access_token(),
                id.signing_alg().map_err(|_| bad())?,
                id.signing_key(&verifier).map_err(|_| bad())?,
            )
            .map_err(|_| bad())?;
            if actual != *expected {
                return Err(bad());
            }
        }
        let user = auth
            .subjects
            .get(claims.subject().as_str())
            .cloned()
            .ok_or_else(bad)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| bad())?
            .as_secs() as i64;
        let expires = claims.expiration().timestamp();
        if claims.issue_time().timestamp() > now || expires <= now {
            return Err(bad());
        }
        let name = claims
            .name()
            .and_then(|name| name.get(None))
            .map(|name| name.as_str().to_owned());
        (user, name, (expires - now).min(900))
    };
    let session = random();
    let mut store = auth.store.lock().map_err(|_| bad())?;
    // A newer login or logout invalidates this browser's in-flight callback.
    if !store
        .bindings
        .remove(&hash(&binding))
        .is_some_and(|expires| expires > Instant::now())
    {
        return Err(bad());
    }
    store.sessions.retain(|_, s| s.expires > Instant::now());
    if store.sessions.len() >= MAX_ENTRIES {
        return Err(Error(
            StatusCode::TOO_MANY_REQUESTS,
            "session capacity reached",
        ));
    }
    if let Some(old) = read_cookie(&headers, auth.cookie_name(SESSION_COOKIE)) {
        store.sessions.remove(&hash(&old));
    }
    if auth.gitlab_auto_signup {
        h.provision_gitlab_user(&auth.issuer, &user)?;
    }
    if let Some(name) = display_name {
        if !name.trim().is_empty() && name.len() <= 96 && !name.chars().any(char::is_control) {
            h.db()?.execute(
                "INSERT OR IGNORE INTO member_profiles(owner,display_name) VALUES (?1,?2)",
                rusqlite::params![user, name.trim()],
            )?;
        }
    }
    store.sessions.insert(
        hash(&session),
        Session {
            binding: hash(&binding),
            user,
            csrf: random(),
            expires: Instant::now() + Duration::from_secs(lifetime as u64),
        },
    );
    let mut response = Redirect::to("/").into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(SESSION_COOKIE, session, lifetime),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(LOGIN_COOKIE, String::new(), 0),
    );
    Ok(response)
}
pub(crate) async fn logout(State(h): State<Hub>, headers: HeaderMap) -> Result<Response, Error> {
    let auth = h.browser_auth.ok_or_else(bad)?;
    if !auth.mutation_allowed(&headers) {
        return Err(bad());
    }
    let mut store = auth.store.lock().map_err(|_| bad())?;
    if let Some(value) = read_cookie(&headers, auth.cookie_name(SESSION_COOKIE)) {
        store.sessions.remove(&hash(&value));
    }
    if let Some(value) = read_cookie(&headers, auth.cookie_name(LOGIN_COOKIE)) {
        store.bindings.remove(&hash(&value));
        store
            .sessions
            .retain(|_, session| session.binding != hash(&value));
        store
            .pending
            .retain(|_, pending| pending.binding != hash(&value));
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(SESSION_COOKIE, String::new(), 0),
    );
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(LOGIN_COOKIE, String::new(), 0),
    );
    Ok(response)
}

#[cfg(test)]
#[path = "browser_auth_tests.rs"]
mod tests;
