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
