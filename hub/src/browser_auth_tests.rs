//! Offline TLS provider: ephemeral keys, pinned test CA, PKCE and single-use codes.
use super::*;
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::TcpListener,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use tower::ServiceExt;

struct Code {
    challenge: String,
    claims: Value,
}
struct Provider {
    origin: String,
    cert: Vec<u8>,
    jwks: Value,
    codes: Arc<Mutex<HashMap<String, Code>>>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for Provider {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker.join().unwrap();
        }
    }
}
fn openssl(args: &[&str]) -> Vec<u8> {
    let output = Command::new("openssl")
        .args(args)
        .output()
        .expect("tests require local OpenSSL");
    assert!(
        output.status.success(),
        "ephemeral test key operation failed"
    );
    output.stdout
}
impl Provider {
    fn new() -> Self {
        let directory =
            std::env::temp_dir().join(format!("agenct-browser-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let key_path = directory.join("key.pem");
        let cert_path = directory.join("cert.pem");
        let key_path = key_path.to_str().unwrap();
        let cert_path = cert_path.to_str().unwrap();
        openssl(&[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            key_path,
            "-out",
            cert_path,
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
            "-addext",
            "subjectAltName=DNS:localhost",
            "-addext",
            "basicConstraints=critical,CA:FALSE",
        ]);
        let signing_key = EncodingKey::from_rsa_pem(&std::fs::read(key_path).unwrap()).unwrap();
        let modulus = openssl(&["rsa", "-in", key_path, "-modulus", "-noout"]);
        let hex = std::str::from_utf8(&modulus)
            .unwrap()
            .trim()
            .strip_prefix("Modulus=")
            .unwrap();
        let modulus: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let cert = openssl(&["x509", "-in", cert_path, "-outform", "DER"]);
        let key = openssl(&[
            "pkcs8", "-topk8", "-nocrypt", "-in", key_path, "-outform", "DER",
        ]);
        std::fs::remove_dir_all(directory).unwrap();
        let crypto = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ServerConfig::builder_with_provider(crypto)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![rustls::pki_types::CertificateDer::from(cert.clone())],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key).into(),
            )
            .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let codes: Arc<Mutex<HashMap<String, Code>>> = Arc::default();
        let stop = Arc::new(AtomicBool::new(false));
        let server_codes = codes.clone();
        let server_stop = stop.clone();
        let worker = thread::spawn(move || {
            while !server_stop.load(Ordering::SeqCst) {
                let (socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let connection = rustls::ServerConnection::new(Arc::new(config.clone())).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, socket);
                let mut request = Vec::new();
                let mut byte = [0u8];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 16384 {
                    if stream.read_exact(&mut byte).is_err() {
                        break;
                    }
                    request.push(byte[0]);
                }
                let headers = String::from_utf8_lossy(&request);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse().unwrap())
                    })
                    .unwrap_or(0);
                if length > 16384 {
                    continue;
                }
                let mut body = vec![0; length];
                if stream.read_exact(&mut body).is_err() {
                    continue;
                }
                let form: HashMap<String, String> = reqwest::Url::parse(&format!(
                    "https://unused/?{}",
                    String::from_utf8_lossy(&body)
                ))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
                let code = form
                    .get("code")
                    .and_then(|code| server_codes.lock().unwrap().remove(code));
                let valid = headers.starts_with("POST /token HTTP/1.1")
                    && form.get("grant_type").map(String::as_str) == Some("authorization_code")
                    && form.get("client_id").map(String::as_str) == Some("browser-client")
                    && form.get("redirect_uri").map(String::as_str)
                        == Some("https://hub.example.test/auth/callback");
                let reply = match code {
                    Some(code)
                        if valid
                            && form.get("code_verifier").map(|verifier| {
                                URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
                            }) == Some(code.challenge.clone()) =>
                    {
                        let mut header = Header::new(Algorithm::RS256);
                        header.kid = Some("ephemeral".into());
                        let token = encode(&header, &code.claims, &signing_key).unwrap();
                        (
                            "200 OK",
                            json!({"access_token":"server-only-test-access-token", "token_type":"Bearer", "expires_in":300, "id_token":token}),
                        )
                    }
                    _ => ("400 Bad Request", json!({"error":"invalid_grant"})),
                };
                let body = reply.1.to_string();
                let response = format!("HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.0, body.len(), body);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            origin,
            cert,
            jwks: json!({"keys":[{"kty":"RSA", "kid":"ephemeral", "use":"sig", "alg":"RS256", "n":URL_SAFE_NO_PAD.encode(modulus), "e":"AQAB"}]}),
            codes,
            stop,
            worker: Some(worker),
        }
    }
    fn hub(&self) -> Hub {
        let http = reqwest::Client::builder()
            .https_only(true)
            .no_proxy()
            .tls_built_in_root_certs(false)
            .add_root_certificate(reqwest::Certificate::from_der(&self.cert).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let auth = BrowserAuth::with_http(
            Config {
                issuer: self.origin.clone(),
                authorization_endpoint: format!("{}/authorize", self.origin),
                token_endpoint: format!("{}/token", self.origin),
                client_id: "browser-client".into(),
                public_origin: "https://hub.example.test".into(),
                subjects: HashMap::from([("subject-alice".into(), "alice".into())]),
                jwks: self.jwks.clone(),
            },
            &HashSet::from(["alice".into()]),
            http,
        )
        .unwrap();
        let mut hub = Hub::open(crate::Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            database: ":memory:".into(),
            users: vec![crate::UserConfig {
                id: "alice".into(),
                token_sha256: hash("test-bearer"),
            }],
            runtime: crate::Runtime::Mock,
        })
        .unwrap();
        hub.browser_auth = Some(auth);
        hub
    }
    async fn begin(&self, hub: &Hub) -> (HeaderMap, Callback) {
        let response = login(State(hub.clone()), HeaderMap::new()).await.unwrap();
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert_cookie(cookie);
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            cookie.split(';').next().unwrap().parse().unwrap(),
        );
        assert!(
            read_cookie(&headers, LOGIN_COOKIE).is_some(),
            "server-generated login cookie must pass its own validation"
        );
        let url =
            reqwest::Url::parse(response.headers()[header::LOCATION].to_str().unwrap()).unwrap();
        let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["response_type"], "code");
        assert!(params["scope"]
            .split_whitespace()
            .any(|scope| scope == "openid"));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let code = random();
        self.codes.lock().unwrap().insert(code.clone(), Code { challenge:params["code_challenge"].clone(), claims:json!({"iss":self.origin,"aud":"browser-client","sub":"subject-alice","nonce":params["nonce"],"iat":now-1,"exp":now+300}) });
        (
            headers,
            Callback {
                code,
                state: params["state"].clone(),
                iss: Some(self.origin.clone()),
            },
        )
    }
}
fn assert_cookie(value: &str) {
    for attr in ["Secure", "HttpOnly", "SameSite=Lax", "Path=/", "Max-Age="] {
        assert!(value.contains(attr), "missing cookie attribute {attr}");
    }
    assert!(!value.contains("Domain="));
}
fn session_headers(response: &Response) -> HeaderMap {
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap())
        .find(|cookie| cookie.starts_with(SESSION_COOKIE))
        .unwrap();
    assert_cookie(cookie);
    let mut headers = HeaderMap::new();
    headers.insert(
        header::COOKIE,
        cookie.split(';').next().unwrap().parse().unwrap(),
    );
    headers.insert(header::HOST, HeaderValue::from_static("hub.example.test"));
    headers
}
async fn request(hub: &Hub, method: &str, path: &str, headers: HeaderMap) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    *request.headers_mut() = headers;
    crate::app(hub.clone()).oneshot(request).await.unwrap()
}
#[tokio::test]
async fn browser_tls_login_session_guards_and_logout() {
    let provider = Provider::new();
    let hub = provider.hub();
    let (headers, query) = provider.begin(&hub).await;
    let response = callback(State(hub.clone()), headers, Query(query))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let mut headers = session_headers(&response);
    assert_eq!(
        hub.browser_auth
            .as_ref()
            .unwrap()
            .identity(&headers)
            .unwrap(),
        "alice"
    );
    let response = request(&hub, "GET", "/auth/session", headers.clone()).await;
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(response.headers()[header::REFERRER_POLICY], "no-referrer");
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(body["user"], "alice");
    assert_eq!(body["authenticated"], true);
    assert!(body.get("access_token").is_none());
    assert!(body.get("id_token").is_none());
    headers.insert(
        "x-csrf-token",
        body["csrf_token"].as_str().unwrap().parse().unwrap(),
    );
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://hub.example.test"),
    );
    assert!(hub
        .browser_auth
        .as_ref()
        .unwrap()
        .mutation_allowed(&headers));
    for name in [
        header::ORIGIN,
        axum::http::HeaderName::from_static("x-csrf-token"),
    ] {
        let mut absent = headers.clone();
        absent.remove(&name);
        assert_eq!(
            request(&hub, "POST", "/auth/logout", absent).await.status(),
            StatusCode::FORBIDDEN
        );
        let mut duplicate = headers.clone();
        duplicate.append(name.clone(), headers[&name].clone());
        assert_eq!(
            request(&hub, "POST", "/auth/logout", duplicate)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut wrong = headers.clone();
    wrong.insert(
        header::ORIGIN,
        HeaderValue::from_static("https://evil.example"),
    );
    assert_eq!(
        request(&hub, "POST", "/auth/logout", wrong).await.status(),
        StatusCode::FORBIDDEN
    );
    let mut wrong = headers.clone();
    wrong.insert("x-csrf-token", HeaderValue::from_static("wrong"));
    assert_eq!(
        request(&hub, "POST", "/auth/logout", wrong).await.status(),
        StatusCode::FORBIDDEN
    );
    for host in [None, Some("evil.example")] {
        let mut wrong = headers.clone();
        wrong.remove(header::HOST);
        if let Some(host) = host {
            wrong.insert(header::HOST, host.parse().unwrap());
        }
        assert_eq!(
            request(&hub, "GET", "/auth/session", wrong).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    let mut bearer = headers.clone();
    bearer.insert(
        header::AUTHORIZATION,
        HeaderValue::from_static("Bearer test-bearer"),
    );
    assert!(hub
        .browser_auth
        .as_ref()
        .unwrap()
        .identity(&bearer)
        .is_err());
    let response = request(&hub, "POST", "/auth/logout", headers.clone()).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .all(|value| value.to_str().unwrap().contains("Max-Age=0")));
    assert!(hub
        .browser_auth
        .as_ref()
        .unwrap()
        .identity(&headers)
        .is_err());
}
#[tokio::test]
async fn browser_rejects_state_binding_replay_and_invalid_pkce() {
    let provider = Provider::new();
    let hub = provider.hub();
    let (headers, query) = provider.begin(&hub).await;
    let attempt = || Callback {
        code: query.code.clone(),
        state: query.state.clone(),
        iss: query.iss.clone(),
    };
    let mut wrong_state = attempt();
    wrong_state.state = random();
    assert!(
        callback(State(hub.clone()), headers.clone(), Query(wrong_state))
            .await
            .is_err()
    );
    assert!(
        callback(State(hub.clone()), HeaderMap::new(), Query(attempt()))
            .await
            .is_err()
    );
    let mut other = HeaderMap::new();
    other.insert(
        header::COOKIE,
        format!("{}={}", LOGIN_COOKIE, random()).parse().unwrap(),
    );
    assert!(callback(State(hub.clone()), other, Query(attempt()))
        .await
        .is_err());
    assert!(
        callback(State(hub.clone()), headers.clone(), Query(attempt()))
            .await
            .is_ok()
    );
    assert!(callback(State(hub.clone()), headers, Query(attempt()))
        .await
        .is_err());
    let (headers, query) = provider.begin(&hub).await;
    provider
        .codes
        .lock()
        .unwrap()
        .get_mut(&query.code)
        .unwrap()
        .challenge = "invalid-pkce".into();
    assert!(callback(State(hub.clone()), headers, Query(query))
        .await
        .is_err());
    assert!(
        provider.codes.lock().unwrap().is_empty(),
        "code must be single use even on invalid PKCE"
    );
}
#[tokio::test]
async fn browser_rejects_invalid_id_token_claims() {
    let provider = Provider::new();
    for (field, value) in [
        ("iss", json!("https://wrong.example")),
        ("aud", json!("wrong-client")),
        ("nonce", json!("wrong-nonce")),
        ("azp", json!("wrong-client")),
        ("sub", json!("unmapped-subject")),
        ("exp", json!(1)),
        ("iat", json!(4102444800u64)),
    ] {
        let hub = provider.hub();
        let (headers, query) = provider.begin(&hub).await;
        provider
            .codes
            .lock()
            .unwrap()
            .get_mut(&query.code)
            .unwrap()
            .claims[field] = value;
        let code = query.code.clone();
        assert!(
            callback(State(hub.clone()), headers, Query(query))
                .await
                .is_err(),
            "accepted invalid {field}"
        );
        assert!(
            !provider.codes.lock().unwrap().contains_key(&code),
            "invalid {field} test must actually exchange its code over TLS"
        );
        assert!(hub
            .browser_auth
            .as_ref()
            .unwrap()
            .store
            .lock()
            .unwrap()
            .sessions
            .is_empty());
    }
    let hub = provider.hub();
    let (headers, mut query) = provider.begin(&hub).await;
    query.iss = Some("https://wrong.example".into());
    assert!(callback(State(hub.clone()), headers, Query(query))
        .await
        .is_err());
}

#[tokio::test]
async fn browser_provider_tls_requires_explicit_test_trust() {
    let provider = Provider::new();
    let client = reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let result = client
        .post(format!("{}/token", provider.origin))
        .body("grant_type=authorization_code")
        .send()
        .await;
    assert!(
        result.is_err(),
        "untrusted ephemeral certificate must not be accepted"
    );
}

#[tokio::test]
async fn browser_rejects_expired_pending_and_session() {
    let provider = Provider::new();
    let hub = provider.hub();
    let (headers, query) = provider.begin(&hub).await;
    hub.browser_auth
        .as_ref()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .pending
        .get_mut(&hash(&query.state))
        .unwrap()
        .expires = Instant::now() - Duration::from_secs(1);
    assert!(callback(State(hub.clone()), headers, Query(query))
        .await
        .is_err());
    let (headers, query) = provider.begin(&hub).await;
    let response = callback(State(hub.clone()), headers, Query(query))
        .await
        .unwrap();
    let headers = session_headers(&response);
    for session in hub
        .browser_auth
        .as_ref()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .sessions
        .values_mut()
    {
        session.expires = Instant::now() - Duration::from_secs(1);
    }
    assert!(hub
        .browser_auth
        .as_ref()
        .unwrap()
        .identity(&headers)
        .is_err());
}

#[tokio::test]
async fn browser_mock_provider_rejects_code_reuse() {
    let provider = Provider::new();
    let hub = provider.hub();
    let (headers, query) = provider.begin(&hub).await;
    let auth = hub.browser_auth.as_ref().unwrap();
    let verifier = auth.store.lock().unwrap().pending[&hash(&query.state)]
        .verifier
        .secret()
        .clone();
    let response = auth
        .http
        .post(format!("{}/token", provider.origin))
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", "browser-client"),
            ("redirect_uri", "https://hub.example.test/auth/callback"),
            ("code", query.code.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(!provider.codes.lock().unwrap().contains_key(&query.code));
    assert!(callback(State(hub.clone()), headers, Query(query))
        .await
        .is_err());
    assert!(auth.store.lock().unwrap().sessions.is_empty());
}

#[tokio::test]
async fn browser_rejects_oversize_token_response_without_session() {
    let provider = Provider::new();
    let hub = provider.hub();
    let (headers, query) = provider.begin(&hub).await;
    provider
        .codes
        .lock()
        .unwrap()
        .get_mut(&query.code)
        .unwrap()
        .claims["oversized"] = json!("x".repeat(140000));
    assert!(callback(State(hub.clone()), headers, Query(query))
        .await
        .is_err());
    assert!(hub
        .browser_auth
        .as_ref()
        .unwrap()
        .store
        .lock()
        .unwrap()
        .sessions
        .is_empty());
}

#[test]
fn browser_rejects_untrusted_configuration() {
    let provider = Provider::new();
    let base = json!({"issuer":provider.origin,"authorization_endpoint":format!("{}/authorize",provider.origin),"token_endpoint":format!("{}/token",provider.origin),"client_id":"client","public_origin":"https://hub.example.test","subjects":{"subject-alice":"alice"},"jwks":provider.jwks});
    for (field, value) in [
        ("public_origin", "http://hub.example.test"),
        ("public_origin", "https://hub.example.test/"),
        ("token_endpoint", "https://other.example.test/token"),
        ("authorization_endpoint", "https://other.example.test/auth"),
        ("issuer", "http://issuer.example.test"),
        ("client_id", ""),
    ] {
        let mut config = base.clone();
        config[field] = json!(value);
        assert!(BrowserAuth::new(
            serde_json::from_value(config).unwrap(),
            &HashSet::from(["alice".to_string()])
        )
        .is_err());
    }
    let mut secret = base;
    secret["client_secret"] = json!("not-supported");
    assert!(serde_json::from_value::<Config>(secret).is_err());
}
