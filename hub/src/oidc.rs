//! Opt-in, offline-pinned RFC 9068 JWT access-token validation.
//! This is not an OIDC login client; no discovery, token exchange, or auto-provisioning.
use base64::Engine;
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub issuer: String,
    pub audience: String,
    pub required_scope: String,
    pub subjects: HashMap<String, String>,
    pub jwks: KeySet,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySet {
    pub keys: Vec<PublicKey>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicKey {
    pub kid: String,
    pub kty: String,
    pub alg: String,
    #[serde(rename = "use")]
    pub usage: String,
    pub n: String,
    pub e: String,
}
#[derive(Clone)]
pub struct Validator {
    issuer: String,
    required_scope: String,
    validation: Validation,
    keys: HashMap<String, DecodingKey>,
    subjects: HashMap<String, String>,
}
// Restrict JOSE headers to this supported profile. In particular, unsupported
// critical extensions and token-controlled key locations are rejected.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AccessHeader {
    alg: Algorithm,
    typ: String,
    kid: String,
}
#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: u64,
    iat: u64,
    nbf: Option<u64>,
    client_id: String,
    scope: String,
    jti: String,
}
impl Validator {
    pub fn new(config: Config, known_users: &HashSet<String>) -> Result<Self, &'static str> {
        let issuer = reqwest::Url::parse(&config.issuer).map_err(|_| "invalid OIDC issuer")?;
        if issuer.scheme() != "https"
            || issuer.host_str().is_none()
            || !issuer.username().is_empty()
            || issuer.password().is_some()
            || issuer.query().is_some()
            || issuer.fragment().is_some()
            || config.audience.trim().is_empty()
            || config.audience.len() > 512
            || config.required_scope.is_empty()
            || config.required_scope.len() > 128
            || !config
                .required_scope
                .bytes()
                .all(|b| b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b))
            || config.subjects.is_empty()
            || config.subjects.len() > 10000
            || config
                .subjects
                .iter()
                .any(|(sub, user)| sub.is_empty() || sub.len() > 255 || !known_users.contains(user))
            || config.jwks.keys.is_empty()
            || config.jwks.keys.len() > 16
        {
            return Err("invalid OIDC policy or subject mapping");
        }
        let mut keys = HashMap::new();
        for key in config.jwks.keys {
            if key.kty != "RSA" || key.alg != "RS256" || key.usage != "sig"
                || key.kid.is_empty() || key.kid.len() > 128 || keys.contains_key(&key.kid)
                // 2048-bit minimum; actual signature/key validation is delegated to ring.
                || key.n.len() < 342 || key.n.len() > 1366 || key.e != "AQAB"
            {
                return Err("invalid OIDC signing key");
            }
            let decoded = DecodingKey::from_rsa_components(&key.n, &key.e)
                .map_err(|_| "invalid OIDC signing key")?;
            keys.insert(key.kid, decoded);
        }
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[&config.issuer]);
        validation.set_audience(&[config.audience]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub", "iat"]);
        validation.validate_nbf = true;
        validation.leeway = 0;
        Ok(Self {
            issuer: config.issuer,
            required_scope: config.required_scope,
            validation,
            keys,
            subjects: config.subjects,
        })
    }
    /// Errors deliberately carry no bearer material or untrusted claims.
    pub fn identity(&self, token: &str) -> Result<String, &'static str> {
        if token.len() > 16384 {
            return Err("invalid access token");
        }
        let encoded_header = token.split('.').next().ok_or("invalid access token")?;
        let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_header)
            .map_err(|_| "invalid access token")?;
        let header: AccessHeader =
            serde_json::from_slice(&header_bytes).map_err(|_| "invalid access token")?;
        if header.alg != Algorithm::RS256
            || !matches!(header.typ.as_str(), "at+jwt" | "application/at+jwt")
        {
            return Err("invalid access token");
        }
        let key = self.keys.get(&header.kid).ok_or("invalid access token")?;
        let claims = decode::<Claims>(token, key, &self.validation)
            .map_err(|_| "invalid access token")?
            .claims;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "invalid access token")?
            .as_secs();
        if claims.iss != self.issuer
            || claims.nbf.is_some_and(|nbf| nbf > now)
            || claims.iat > now
            || claims.exp <= now
            || claims.exp <= claims.iat
            || claims.exp - claims.iat > 3600
            || !claims
                .scope
                .split(' ')
                .any(|scope| scope == self.required_scope)
            || claims.client_id.is_empty()
            || claims.jti.is_empty()
        {
            return Err("invalid access token");
        }
        self.subjects
            .get(&claims.sub)
            .cloned()
            .ok_or("invalid access token")
    }
}
