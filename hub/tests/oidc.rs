use agenct_hub::{app, oidc, Config, Hub, Runtime, UserConfig};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use base64::Engine;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    process::{Command, Stdio},
    sync::OnceLock,
};
use tower::ServiceExt;
struct MockProvider {
    signing_key: EncodingKey,
    modulus: String,
}
fn provider() -> &'static MockProvider {
    static PROVIDER: OnceLock<MockProvider> = OnceLock::new();
    PROVIDER.get_or_init(|| {
        // Ephemeral test material only: never written to files or printed.
        let generated = Command::new("openssl")
            .args([
                "genpkey",
                "-algorithm",
                "RSA",
                "-pkeyopt",
                "rsa_keygen_bits:2048",
                "-outform",
                "DER",
            ])
            .output()
            .expect("OIDC tests require OpenSSL on PATH");
        assert!(
            generated.status.success(),
            "ephemeral RSA generation failed"
        );
        let mut public = Command::new("openssl")
            .args(["rsa", "-inform", "DER", "-modulus", "-noout"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("OIDC tests require OpenSSL on PATH");
        public
            .stdin
            .take()
            .unwrap()
            .write_all(&generated.stdout)
            .unwrap();
        let output = public.wait_with_output().unwrap();
        assert!(output.status.success(), "test public key extraction failed");
        let text = std::str::from_utf8(&output.stdout).unwrap().trim();
        let hex = text
            .strip_prefix("Modulus=")
            .expect("OpenSSL modulus output");
        let modulus: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        MockProvider {
            signing_key: EncodingKey::from_rsa_der(&generated.stdout),
            modulus: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(modulus),
        }
    })
}
fn policy() -> Value {
    let mut value: Value =
        serde_json::from_str(include_str!("fixtures/oidc-test-only.json")).unwrap();
    value["jwks"]["keys"][0]["n"] = json!(provider().modulus);
    value
}
fn validator(value: Value) -> Result<oidc::Validator, &'static str> {
    oidc::Validator::new(
        serde_json::from_value(value).unwrap(),
        &["alice".to_string()].into(),
    )
}
fn claims() -> Value {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    json!({"iss":"https://issuer.example.test","aud":"agenct-api","sub":"subject-alice","iat":now-1,"exp":now+300,"scope":"agenct:access","client_id":"test-client","jti":"test-token"})
}
fn header() -> Header {
    let mut h = Header::new(Algorithm::RS256);
    h.typ = Some("at+jwt".into());
    h.kid = Some("test-key".into());
    h
}
fn token(h: Header, c: Value) -> String {
    encode(&h, &c, &provider().signing_key).unwrap()
}
#[test]
fn validates_pinned_mock_provider() {
    assert_eq!(
        validator(policy())
            .unwrap()
            .identity(&token(header(), claims()))
            .unwrap(),
        "alice"
    );
}
#[test]
fn rejects_claim_confusion_expiry_and_unmapped_subject() {
    let validator = validator(policy()).unwrap();
    for (field, value) in [
        ("iss", json!("https://evil.test")),
        ("aud", json!("other-api")),
        ("sub", json!("alice")),
        ("exp", json!(1)),
        ("iat", json!(9999999999u64)),
        ("nbf", json!(9999999999u64)),
        ("client_id", json!("")),
        ("jti", json!("")),
        ("scope", json!("other:access")),
    ] {
        let mut c = claims();
        c[field] = value;
        assert!(
            validator.identity(&token(header(), c)).is_err(),
            "accepted invalid {field}"
        );
    }
    for field in [
        "iss",
        "aud",
        "sub",
        "exp",
        "iat",
        "client_id",
        "jti",
        "scope",
    ] {
        let mut c = claims();
        c.as_object_mut().unwrap().remove(field);
        assert!(
            validator.identity(&token(header(), c)).is_err(),
            "accepted missing {field}"
        );
    }
    let mut c = claims();
    c["exp"] = json!(c["iat"].as_u64().unwrap() + 3601);
    assert!(validator.identity(&token(header(), c)).is_err());
}
#[test]
fn rejects_key_algorithm_and_token_type_confusion() {
    let v = validator(policy()).unwrap();
    let mut h = header();
    h.kid = Some("unknown".into());
    assert!(v.identity(&token(h, claims())).is_err());
    let mut h = header();
    h.typ = Some("JWT".into());
    assert!(v.identity(&token(h, claims())).is_err());
    let mut h = header();
    h.jku = Some("https://evil.test/keys".into());
    assert!(v.identity(&token(h, claims())).is_err());
    let mut h = header();
    h.alg = Algorithm::HS256;
    let forged = encode(
        &h,
        &claims(),
        &EncodingKey::from_secret(b"arbitrary secret"),
    )
    .unwrap();
    assert!(v.identity(&forged).is_err());
    let valid = token(header(), claims());
    let mut parts: Vec<_> = valid.split('.').map(str::to_owned).collect();
    let replacement = if &parts[2][0..1] == "A" { "B" } else { "A" };
    parts[2].replace_range(0..1, replacement);
    assert!(v.identity(&parts.join(".")).is_err());
    assert!(v.identity(&"x".repeat(16385)).is_err());
}
#[test]
fn invalid_configuration_fails_closed() {
    for field in ["issuer", "audience"] {
        let mut p = policy();
        p[field] = json!("");
        assert!(validator(p).is_err());
    }
    let mut p = policy();
    p["issuer"] = json!("http://localhost");
    assert!(validator(p).is_err());
    let mut p = policy();
    p["subjects"]["subject-alice"] = json!("unprovisioned");
    assert!(validator(p).is_err());
    let mut p = policy();
    let key = p["jwks"]["keys"][0].clone();
    p["jwks"]["keys"].as_array_mut().unwrap().push(key);
    assert!(validator(p).is_err());
    let mut p = policy();
    p["jwks"]["keys"][0]["alg"] = json!("HS256");
    assert!(validator(p).is_err());
}
#[tokio::test]
async fn opt_in_replaces_dev_tokens_without_fallback() {
    let dev = "development-token-that-is-at-least-32-characters";
    let h = Hub::open(Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: ":memory:".into(),
        users: vec![UserConfig {
            id: "alice".into(),
            token_sha256: format!("{:x}", Sha256::digest(dev)),
        }],
        runtime: Runtime::Mock,
    })
    .unwrap();
    let app = app(h
        .with_oidc(serde_json::from_value(policy()).unwrap())
        .unwrap());
    for (bearer, expected) in [
        (dev.to_owned(), StatusCode::UNAUTHORIZED),
        (token(header(), claims()), StatusCode::OK),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/me")
                    .header("authorization", format!("Bearer {bearer}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
    }
}

#[test]
fn issuer_must_be_one_exact_string_not_a_collection() {
    let v = validator(policy()).unwrap();
    for issuer in [
        json!(["https://issuer.example.test"]),
        json!(["https://evil.test", "https://issuer.example.test"]),
        json!("https://issuer.example.test/"),
        json!("https://ISSUER.example.test"),
        json!(null),
        json!(42),
    ] {
        let mut c = claims();
        c["iss"] = issuer;
        assert!(v.identity(&token(header(), c)).is_err());
    }
    assert_eq!(v.identity(&token(header(), claims())).unwrap(), "alice");
}

fn organization_config(database: &str, runtime: Runtime) -> Config {
    Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        database: database.into(),
        runtime,
        users: ["alice", "bob", "carol", "dave"]
            .into_iter()
            .map(|id| UserConfig {
                id: id.into(),
                token_sha256: format!(
                    "{:x}",
                    Sha256::digest(format!("dev-token-{id}-{}", "x".repeat(40)))
                ),
            })
            .collect(),
    }
}
fn organization_hub(database: &str, runtime: Runtime) -> Hub {
    let mut config = policy();
    config["subjects"] = json!({"subject-alice":"alice","subject-bob":"bob","subject-carol":"carol","subject-dave":"dave"});
    Hub::open(organization_config(database, runtime))
        .unwrap()
        .with_oidc(serde_json::from_value(config).unwrap())
        .unwrap()
}
fn organizations() -> agenct_hub::tenants::Config {
    serde_json::from_value(json!({"alice":"north","bob":"north","carol":"south","dave":"south"}))
        .unwrap()
}
async fn organization_request(
    app: &axum::Router,
    who: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    use http_body_util::BodyExt;
    let mut identity = claims();
    identity["sub"] = json!(format!("subject-{who}"));
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header(
                    "authorization",
                    format!("Bearer {}", token(header(), identity)),
                )
                .header("content-type", "application/json")
                .body(if method == "GET" {
                    Body::empty()
                } else {
                    Body::from(body.to_string())
                })
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn oidc_tenants_isolate_proposals_tasks_drafts_and_history() {
    let a = app(organization_hub(":memory:", Runtime::Mock)
        .with_tenants(organizations())
        .unwrap());
    for (who, tenant) in [("alice", "north"), ("carol", "south")] {
        let (status, me) = organization_request(&a, who, "GET", "/api/me", Value::Null).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(me["tenant"], tenant);
        assert_eq!(
            organization_request(
                &a,
                who,
                "POST",
                "/api/messages",
                json!({"request_id":"same","message":format!("private-{who}")})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for (from, to) in [("alice", "carol"), ("carol", "bob")] {
        assert_eq!(
            organization_request(
                &a,
                from,
                "POST",
                "/api/proposals",
                json!({"owner":to,"summary":"cross boundary"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (status, proposal) = organization_request(
        &a,
        "alice",
        "POST",
        "/api/proposals",
        json!({"owner":"bob","summary":"Same organization"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let action = format!(
        "/api/proposals/{}/actions",
        proposal["id"].as_str().unwrap()
    );
    assert_eq!(
        organization_request(
            &a,
            "alice",
            "POST",
            &action,
            json!({"action":"confirm","expected_revision":0,"owner":"carol"})
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        organization_request(
            &a,
            "carol",
            "POST",
            &action,
            json!({"action":"confirm","expected_revision":0})
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (status, confirmed) = organization_request(
        &a,
        "alice",
        "POST",
        &action,
        json!({"action":"confirm","expected_revision":0}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let task = confirmed["task_id"].as_str().unwrap();
    let actions = format!("/api/tasks/{task}/actions");
    assert_eq!(
        organization_request(
            &a,
            "bob",
            "POST",
            &actions,
            json!({"action":"accept","expected_revision":0})
        )
        .await
        .0,
        StatusCode::OK
    );
    let drafts = format!("/api/tasks/{task}/drafts");
    assert_eq!(
        organization_request(
            &a,
            "bob",
            "POST",
            &drafts,
            json!({"request_id":"private-draft","expected_revision":1})
        )
        .await
        .0,
        StatusCode::OK
    );
    for who in ["carol", "dave"] {
        for path in [
            format!("/api/tasks/{task}"),
            drafts.clone(),
            format!("{drafts}/turns/private-draft"),
        ] {
            assert_eq!(
                organization_request(&a, who, "GET", &path, Value::Null)
                    .await
                    .0,
                StatusCode::NOT_FOUND
            );
        }
        for path in ["/api/tasks", "/api/proposals", "/api/activity"] {
            assert_eq!(
                organization_request(&a, who, "GET", path, Value::Null)
                    .await
                    .1,
                json!([])
            );
        }
        assert_eq!(
            organization_request(
                &a,
                who,
                "POST",
                &drafts,
                json!({"request_id":"foreign","expected_revision":1})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            organization_request(
                &a,
                who,
                "POST",
                &actions,
                json!({"action":"deliver","expected_revision":1,"result":"forged"})
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        let history = organization_request(&a, who, "GET", "/api/messages", Value::Null)
            .await
            .1
            .to_string();
        assert!(!history.contains("private-alice"));
        assert!(!history.contains("private-draft"));
    }
    let receipt = organization_request(&a, "carol", "GET", "/api/turns/same", Value::Null)
        .await
        .1
        .to_string();
    assert!(!receipt.contains("private-alice"));
    assert_eq!(
        organization_request(&a, "alice", "GET", &drafts, Value::Null)
            .await
            .0,
        StatusCode::NOT_FOUND,
        "same organization does not grant private draft access"
    );
}

#[test]
fn tenant_startup_requires_oidc_and_rejects_cross_organization_legacy_records() {
    assert!(Hub::open(organization_config(":memory:", Runtime::Mock))
        .unwrap()
        .with_tenants(organizations())
        .is_err());
    for kind in ["task", "proposal"] {
        let root = std::env::temp_dir().join(format!("agenct-tenant-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let database = root.join("hub.sqlite3");
        let h = organization_hub(database.to_str().unwrap(), Runtime::Mock);
        let db = rusqlite::Connection::open(&database).unwrap();
        if kind == "task" {
            db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('old','alice','carol','old data','awaiting_owner',0)",[]).unwrap();
        } else {
            db.execute("INSERT INTO proposals(id,requester,owner,summary,status,revision,source,request_id) VALUES ('old','alice','carol','old data','pending',0,'manual','old-request')",[]).unwrap();
        }
        assert!(h.with_tenants(organizations()).is_err());
        assert_eq!(
            db.query_row(
                if kind == "task" {
                    "SELECT count(*) FROM tasks"
                } else {
                    "SELECT count(*) FROM proposals"
                },
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
    let h = organization_hub(":memory:", Runtime::Mock)
        .with_tenants(organizations())
        .unwrap();
    assert!(h.with_private_config("/unused".into()).is_err());
}

#[tokio::test]
async fn unavailable_recipient_notice_only_lists_own_organization() {
    let adapter=axum::Router::new().route("/turn",axum::routing::post(|| async {
        axum::Json(json!({"reply":"proposal","backend":"dsh","proposal":{"owner":"carol","summary":"review"}}))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/turn", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, adapter).await.unwrap() });
    let a = app(organization_hub(
        ":memory:",
        Runtime::Dsh {
            url,
            token: "internal-token".into(),
        },
    )
    .with_tenants(organizations())
    .unwrap());
    let (status, result) = organization_request(
        &a,
        "alice",
        "POST",
        "/api/messages",
        json!({"request_id":"notice","message":"ask carol to review"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reply = result["reply"].as_str().unwrap();
    assert!(reply.contains("bob"));
    assert!(!reply.contains("dave"));
    assert!(result["proposal"].is_null());
    server.abort();
}

#[tokio::test]
async fn persisted_tenant_assignments_reject_disabled_mode_and_reassignment_after_restart() {
    let root =
        std::env::temp_dir().join(format!("agenct-persisted-tenants-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let database = root.join("hub.sqlite3");
    let path = database.to_str().unwrap();
    {
        let h = organization_hub(path, Runtime::Mock)
            .with_tenants(organizations())
            .unwrap();
        assert!(h.validate_startup().is_ok());
        let a = app(h);
        assert_eq!(
            organization_request(
                &a,
                "alice",
                "POST",
                "/api/messages",
                json!({"request_id":"durable-private","message":"private-persisted-alice"})
            )
            .await
            .0,
            StatusCode::OK
        );
    }
    for oidc in [false, true] {
        let h = if oidc {
            organization_hub(path, Runtime::Mock)
        } else {
            Hub::open(organization_config(path, Runtime::Mock)).unwrap()
        };
        assert!(
            h.validate_startup().is_err(),
            "missing explicit tenant config must prevent startup"
        );
        let a = app(h);
        for route in [
            "/api/me",
            "/api/messages",
            "/api/tasks",
            "/api/repositories",
        ] {
            assert_eq!(
                organization_request(&a, "alice", "GET", route, Value::Null)
                    .await
                    .0,
                StatusCode::SERVICE_UNAVAILABLE,
                "library callers also fail closed"
            );
        }
    }
    let mut moved = organizations();
    for tenant in moved.0.values_mut() {
        *tenant = format!("new-{tenant}");
    }
    assert!(
        organization_hub(path, Runtime::Mock)
            .with_tenants(moved)
            .is_err(),
        "moving both participants together must still be refused"
    );
    {
        let h = organization_hub(path, Runtime::Mock)
            .with_tenants(organizations())
            .unwrap();
        assert!(h.validate_startup().is_ok());
        let a = app(h);
        assert!(
            organization_request(&a, "alice", "GET", "/api/messages", Value::Null)
                .await
                .1
                .to_string()
                .contains("private-persisted-alice")
        );
        assert!(
            !organization_request(&a, "carol", "GET", "/api/messages", Value::Null)
                .await
                .1
                .to_string()
                .contains("private-persisted-alice")
        );
        let db = rusqlite::Connection::open(&database).unwrap();
        for sql in [
            "UPDATE tenant_memberships SET tenant='other' WHERE owner='alice'",
            "DELETE FROM tenant_memberships WHERE owner='alice'",
            "INSERT OR REPLACE INTO tenant_memberships(owner,tenant) VALUES ('alice','other')",
        ] {
            assert!(db.execute(sql, []).is_err());
        }
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn orphaned_private_history_cannot_be_silently_adopted_at_first_tenant_startup() {
    let root = std::env::temp_dir().join(format!("agenct-orphan-tenants-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let path = root.join("hub.sqlite3");
    let h = organization_hub(path.to_str().unwrap(), Runtime::Mock);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute(
        "INSERT INTO sessions(owner,session_id) VALUES ('former-user','former-session')",
        [],
    )
    .unwrap();
    db.execute("INSERT INTO messages(owner,role,content,status) VALUES ('former-user','user','private old data','complete')",[]).unwrap();
    assert!(h.with_tenants(organizations()).is_err());
    assert_eq!(
        db.query_row("SELECT count(*) FROM tenant_memberships", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0,
        "failed startup must roll back partially bound identities"
    );
    drop(db);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn departed_member_history_remains_bound_without_new_delegation_authority() {
    let root = std::env::temp_dir().join(format!("agenct-departed-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let database = root.join("hub.sqlite3");
    let path = database.to_str().unwrap();
    {
        let h = organization_hub(path, Runtime::Mock)
            .with_tenants(organizations())
            .unwrap();
        let db = rusqlite::Connection::open(&database).unwrap();
        db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('departed-task','alice','bob','Historical work','completed',2)", []).unwrap();
        db.execute("INSERT INTO proposals(id,requester,owner,summary,status,revision,source,request_id) VALUES ('departed-proposal','alice','bob','Historical proposal','pending',0,'manual','departed')", []).unwrap();
        drop(db);
        drop(h);
    }
    let reopen = || {
        let mut config = organization_config(path, Runtime::Mock);
        config.users.retain(|user| user.id != "bob");
        let mut oidc = policy();
        oidc["subjects"] =
            json!({"subject-alice":"alice","subject-carol":"carol","subject-dave":"dave"});
        let mut tenants = organizations();
        tenants.0.remove("bob");
        Hub::open(config)
            .unwrap()
            .with_oidc(serde_json::from_value(oidc).unwrap())
            .unwrap()
            .with_tenants(tenants)
    };
    {
        let a = app(reopen().unwrap());
        assert_eq!(
            organization_request(&a, "alice", "GET", "/api/tasks/departed-task", Value::Null)
                .await
                .0,
            StatusCode::OK
        );
        assert_eq!(
            organization_request(&a, "bob", "GET", "/api/tasks/departed-task", Value::Null)
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            organization_request(
                &a,
                "alice",
                "POST",
                "/api/proposals",
                json!({"owner":"bob","summary":"New work"})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            organization_request(
                &a,
                "alice",
                "POST",
                "/api/proposals/departed-proposal/actions",
                json!({"action":"confirm","expected_revision":0})
            )
            .await
            .0,
            StatusCode::BAD_REQUEST
        );
    }
    {
        let db = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            db.query_row(
                "SELECT tenant FROM tenant_memberships WHERE owner='bob'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "north"
        );
        assert_eq!(
            db.query_row(
                "SELECT status FROM proposals WHERE id='departed-proposal'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
            "pending"
        );
        // Retained membership must not excuse historical cross-tenant records.
        db.execute("INSERT INTO tasks(id,requester,owner,summary,status,revision) VALUES ('cross-tenant','carol','bob','Invalid history','completed',2)", []).unwrap();
    }
    assert!(reopen().is_err());
    std::fs::remove_dir_all(root).unwrap();
}
