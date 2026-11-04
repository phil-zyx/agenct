//! Opt-in, static-provider OIDC browser sessions. No discovery or auto-provisioning.
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
impl BrowserAuth {
    pub fn new(config: Config, known: &HashSet<String>) -> Result<Self, &'static str> {
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| "TLS client unavailable")?;
        Self::with_http(config, known, http)
    }
    fn with_http(
        config: Config,
        known: &HashSet<String>,
        http: reqwest::Client,
    ) -> Result<Self, &'static str> {
        let issuer = https(&config.issuer)?;
        let authorization = https(&config.authorization_endpoint)?;
        let token = https(&config.token_endpoint)?;
        let origin = https(&config.public_origin)?;
        if authorization.origin() != issuer.origin()
            || token.origin() != issuer.origin()
            || origin.path() != "/"
            || config.public_origin != origin.origin().ascii_serialization()
            || config.client_id.is_empty()
            || config.client_id.len() > 256
            || config.subjects.is_empty()
            || config
                .subjects
                .iter()
                .any(|(sub, user)| sub.is_empty() || sub.len() > 255 || !known.contains(user))
        {
            return Err("invalid browser OIDC trust policy");
        }
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
        let jwks: CoreJsonWebKeySet =
            serde_json::from_value(config.jwks).map_err(|_| "invalid pinned public keys")?;
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
            RedirectUrl::new(format!("{}/auth/callback", config.public_origin))
                .map_err(|_| "invalid callback")?,
        );
        let authority = config
            .public_origin
            .strip_prefix("https://")
            .ok_or("invalid origin")?
            .to_owned();
        Ok(Self {
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
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let value = read_cookie(headers, SESSION_COOKIE)?;
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
    let Some(auth) = h.browser_auth else {
        return Json(serde_json::json!({"enabled":false}));
    };
    match auth.session(&headers) {
        Some(s) => Json(
            serde_json::json!({"enabled":true,"authenticated":true,"user":s.user,"csrf_token":s.csrf}),
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
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .set_pkce_challenge(challenge)
        .url();
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
    if let Some(old) = read_cookie(&headers, LOGIN_COOKIE) {
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
        .append(header::SET_COOKIE, cookie(LOGIN_COOKIE, binding, 300));
    Ok(response)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Callback {
    code: String,
    state: String,
    iss: Option<String>,
}
pub(crate) async fn callback(
    State(h): State<Hub>,
    headers: HeaderMap,
    Query(query): Query<Callback>,
) -> Result<Response, Error> {
    let auth = h.browser_auth.ok_or_else(bad)?;
    if query.code.len() > 4096 || query.state.len() > 256 {
        return Err(bad());
    }
    let binding = read_cookie(&headers, LOGIN_COOKIE).ok_or_else(bad)?;
    let pending = {
        let mut store = auth.store.lock().map_err(|_| bad())?;
        let key = hash(&query.state);
        let pending = store.pending.get(&key).ok_or_else(bad)?;
        if pending.binding != hash(&binding) || pending.expires <= Instant::now() {
            return Err(bad());
        }
        store.pending.remove(&key).ok_or_else(bad)?
    };
    if query
        .iss
        .as_ref()
        .is_some_and(|iss| iss != auth.issuer.as_str())
    {
        return Err(bad());
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
        .map_err(|_| bad())?;
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
    let lifetime = (expires - now).min(900);
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
    if let Some(old) = read_cookie(&headers, SESSION_COOKIE) {
        store.sessions.remove(&hash(&old));
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
        cookie(SESSION_COOKIE, session, lifetime),
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie(LOGIN_COOKIE, String::new(), 0));
    Ok(response)
}
pub(crate) async fn logout(State(h): State<Hub>, headers: HeaderMap) -> Result<Response, Error> {
    let auth = h.browser_auth.ok_or_else(bad)?;
    if !auth.mutation_allowed(&headers) {
        return Err(bad());
    }
    let mut store = auth.store.lock().map_err(|_| bad())?;
    if let Some(value) = read_cookie(&headers, SESSION_COOKIE) {
        store.sessions.remove(&hash(&value));
    }
    if let Some(value) = read_cookie(&headers, LOGIN_COOKIE) {
        store.bindings.remove(&hash(&value));
        store
            .sessions
            .retain(|_, session| session.binding != hash(&value));
        store
            .pending
            .retain(|_, pending| pending.binding != hash(&value));
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie(SESSION_COOKIE, String::new(), 0));
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie(LOGIN_COOKIE, String::new(), 0));
    Ok(response)
}

#[cfg(test)]
#[path = "browser_auth_tests.rs"]
mod tests;
