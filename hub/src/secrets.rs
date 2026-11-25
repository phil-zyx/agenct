//! Optional AEAD (AES-256-GCM) encryption at rest for per-user model & integration secrets.
use crate::{Error, Result};
use axum::http::StatusCode;
use base64::{engine::general_purpose::STANDARD, Engine};
use ring::{
    aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN},
    hkdf::{KeyType, Salt, HKDF_SHA256},
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::params;
use serde::{de::DeserializeOwned, Serialize};
use std::{fmt, sync::Arc};

const PREFIX: &str = "enc:v1:";
const HKDF_SALT: &[u8] = b"agenct-v2-master-key-salt-v1";
const HKDF_INFO: &[u8] = b"agenct-v2-secrets-aes256gcm-v1";

pub const SCOPE_USER_MODELS: &str = "user_models";
pub const SCOPE_GITLAB: &str = "integration:gitlab";
pub const SCOPE_IM: &str = "integration:im";

struct Aes256KeyLen;

impl KeyType for Aes256KeyLen {
    fn len(&self) -> usize {
        32
    }
}

/// Symmetric cipher protecting persisted user credentials when `AGENCT_MASTER_KEY` is configured.
/// When disabled, reads and writes pass through as plaintext while preserving backward compatibility.
#[derive(Clone, Default)]
pub struct SecretCipher {
    key: Option<Arc<LessSafeKey>>,
}

impl fmt::Debug for SecretCipher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.key.is_some() {
            f.write_str("SecretCipher(enabled)")
        } else {
            f.write_str("SecretCipher(disabled)")
        }
    }
}

impl SecretCipher {
    /// Builds a cipher from an optional `AGENCT_MASTER_KEY` string.
    /// When `None`, encryption at rest is disabled (legacy plaintext mode).
    pub fn from_raw(raw: Option<&str>) -> std::result::Result<Self, &'static str> {
        let Some(raw) = raw else {
            return Ok(Self { key: None });
        };
        let trimmed = raw.trim();
        if trimmed.len() < 32 || trimmed.len() > 4096 || trimmed.chars().any(char::is_control) {
            return Err("AGENCT_MASTER_KEY must be 32..4096 non-control characters");
        }
        let salt = Salt::new(HKDF_SHA256, HKDF_SALT);
        let prk = salt.extract(trimmed.as_bytes());
        let okm = prk
            .expand(&[HKDF_INFO], Aes256KeyLen)
            .map_err(|_| "master key derivation failed")?;
        let mut key_bytes = [0u8; 32];
        okm.fill(&mut key_bytes)
            .map_err(|_| "master key derivation failed")?;
        let unbound =
            UnboundKey::new(&AES_256_GCM, &key_bytes).map_err(|_| "master key init failed")?;
        Ok(Self {
            key: Some(Arc::new(LessSafeKey::new(unbound))),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.key.is_some()
    }

    pub fn is_encrypted(stored: &str) -> bool {
        stored.starts_with(PREFIX)
    }

    fn aad(scope: &str, user_id: &str) -> Aad<Vec<u8>> {
        Aad::from(format!("agenct:v1:{scope}:{user_id}").into_bytes())
    }

    /// Encrypts `plaintext` when a master key is active, binding `(scope, user_id)` in AAD.
    pub fn encrypt(&self, scope: &str, user_id: &str, plaintext: &str) -> Result<String> {
        let Some(ref key) = self.key else {
            return Ok(plaintext.to_owned());
        };
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "rng unavailable"))?;
        let mut in_out = plaintext.as_bytes().to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Self::aad(scope, user_id),
            &mut in_out,
        )
        .map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "secret encryption failed",
            )
        })?;
        let mut combined = Vec::with_capacity(NONCE_LEN + in_out.len());
        combined.extend_from_slice(&nonce_bytes);
        combined.extend_from_slice(&in_out);
        Ok(format!("{PREFIX}{}", STANDARD.encode(combined)))
    }

    /// Decrypts `stored` if prefixed with `enc:v1:`, or returns legacy plaintext untouched.
    pub fn decrypt(&self, scope: &str, user_id: &str, stored: &str) -> Result<String> {
        let Some(b64) = stored.strip_prefix(PREFIX) else {
            return Ok(stored.to_owned());
        };
        let Some(ref key) = self.key else {
            return Err(Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "encrypted secret requires AGENCT_MASTER_KEY",
            ));
        };
        let raw = STANDARD
            .decode(b64)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "secret decode failed"))?;
        if raw.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return Err(Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "secret payload truncated",
            ));
        }
        let (nonce_slice, ciphertext) = raw.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce_slice)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "invalid secret nonce"))?;
        let mut buf = ciphertext.to_vec();
        let plain = key
            .open_in_place(nonce, Self::aad(scope, user_id), &mut buf)
            .map_err(|_| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "secret decryption failed",
                )
            })?;
        String::from_utf8(plain.to_vec()).map_err(|_| {
            Error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "secret utf8 decode failed",
            )
        })
    }

    pub fn encrypt_json<T: Serialize>(
        &self,
        scope: &str,
        user_id: &str,
        value: &T,
    ) -> Result<String> {
        let json = serde_json::to_string(value)
            .map_err(|_| Error(StatusCode::INTERNAL_SERVER_ERROR, "secret encode error"))?;
        self.encrypt(scope, user_id, &json)
    }

    /// Decrypts and deserializes an optional `config_json` cell, preserving legacy plaintext fallback.
    pub fn decrypt_json<T: DeserializeOwned + Default>(
        &self,
        scope: &str,
        user_id: &str,
        stored: Option<String>,
    ) -> Result<T> {
        let Some(raw) = stored else {
            return Ok(T::default());
        };
        let was_enc = Self::is_encrypted(&raw);
        let plain = self.decrypt(scope, user_id, &raw)?;
        if was_enc {
            serde_json::from_str(&plain).map_err(|_| {
                Error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "stored secret decode error",
                )
            })
        } else {
            Ok(serde_json::from_str(&plain).unwrap_or_default())
        }
    }
}

/// Re-encrypts `user_models` and `integrations` rows into `next` inside one transaction.
/// When `old` is supplied, rotates rows encrypted under `old` to `next` while leaving rows
/// already encrypted under `next` untouched.
pub fn reencrypt_db(
    conn: &mut rusqlite::Connection,
    next: &SecretCipher,
    old: Option<&SecretCipher>,
) -> Result<usize> {
    if !next.is_enabled() {
        return Err(crate::invalid(
            "AGENCT_MASTER_KEY is required to re-encrypt secrets",
        ));
    }
    let decrypt_row = |scope: &str, uid: &str, raw: &str| -> Result<(String, bool)> {
        if !SecretCipher::is_encrypted(raw) {
            return Ok((raw.to_owned(), true));
        }
        if let Some(prev) = old {
            if let Ok(plain) = prev.decrypt(scope, uid, raw) {
                return Ok((plain, true));
            }
            next.decrypt(scope, uid, raw)?;
            return Ok((String::new(), false));
        }
        next.decrypt(scope, uid, raw)?;
        Ok((String::new(), false))
    };

    let tx = conn.transaction()?;
    let now = crate::config::now_unix();
    let mut updated = 0usize;

    {
        let mut stmt = tx.prepare("SELECT user_id, config_json FROM user_models")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (uid, raw) in rows {
            let (plain, needs_write) = decrypt_row(SCOPE_USER_MODELS, &uid, &raw)?;
            if needs_write {
                let enc = next.encrypt(SCOPE_USER_MODELS, &uid, &plain)?;
                tx.execute(
                    "UPDATE user_models SET config_json = ?2, updated_at = ?3 WHERE user_id = ?1",
                    params![uid, enc, now],
                )?;
                updated += 1;
            }
        }
    }

    {
        let mut stmt = tx.prepare(
            "SELECT user_id, kind, config_json FROM integrations WHERE kind IN ('gitlab', 'im')",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (uid, kind, raw) in rows {
            let scope = match kind.as_str() {
                "gitlab" => SCOPE_GITLAB,
                "im" => SCOPE_IM,
                _ => continue,
            };
            let (plain, needs_write) = decrypt_row(scope, &uid, &raw)?;
            if needs_write {
                let enc = next.encrypt(scope, &uid, &plain)?;
                tx.execute(
                    "UPDATE integrations SET config_json = ?3, updated_at = ?4
                     WHERE user_id = ?1 AND kind = ?2",
                    params![uid, kind, enc, now],
                )?;
                updated += 1;
            }
        }
    }

    tx.commit()?;
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: &str = "alpha-master-key-0123456789abcdef012345";
    const KEY_B: &str = "bravo-master-key-0123456789abcdef012345";

    #[test]
    fn key_validation_and_debug_redaction() {
        assert!(!SecretCipher::from_raw(None).unwrap().is_enabled());
        assert!(SecretCipher::from_raw(Some("too-short")).is_err());
        assert!(SecretCipher::from_raw(Some("0123456789abcdef\n0123456789abcdef")).is_err());
        assert!(SecretCipher::from_raw(Some(&format!("{KEY_A}\n")))
            .unwrap()
            .is_enabled());
        let cipher = SecretCipher::from_raw(Some(KEY_A)).unwrap();
        assert!(cipher.is_enabled());
        assert_eq!(format!("{cipher:?}"), "SecretCipher(enabled)");
    }

    #[test]
    fn encrypt_decrypt_round_trip_and_aad_isolation() {
        let c1 = SecretCipher::from_raw(Some(KEY_A)).unwrap();
        let c2 = SecretCipher::from_raw(Some(KEY_B)).unwrap();
        let none = SecretCipher::default();

        let e1 = c1
            .encrypt(SCOPE_USER_MODELS, "u-alice", r#"{"api_key":"sk-secret"}"#)
            .unwrap();
        let e2 = c1
            .encrypt(SCOPE_USER_MODELS, "u-alice", r#"{"api_key":"sk-secret"}"#)
            .unwrap();
        assert!(e1.starts_with("enc:v1:"));
        assert_ne!(e1, e2, "random nonce must produce distinct ciphertexts");
        assert!(!e1.contains("sk-secret"));

        assert_eq!(
            c1.decrypt(SCOPE_USER_MODELS, "u-alice", &e1).unwrap(),
            r#"{"api_key":"sk-secret"}"#
        );
        // Plaintext fallback remains readable even when cipher is enabled.
        assert_eq!(
            c1.decrypt(SCOPE_USER_MODELS, "u-alice", r#"{"api_key":"legacy"}"#)
                .unwrap(),
            r#"{"api_key":"legacy"}"#
        );
        // Wrong key, disabled cipher, wrong user_id AAD, wrong scope AAD, and tampering must fail.
        assert!(c2.decrypt(SCOPE_USER_MODELS, "u-alice", &e1).is_err());
        assert!(none.decrypt(SCOPE_USER_MODELS, "u-alice", &e1).is_err());
        assert!(c1.decrypt(SCOPE_USER_MODELS, "u-bob", &e1).is_err());
        assert!(c1.decrypt(SCOPE_GITLAB, "u-alice", &e1).is_err());

        let mut tampered = e1.clone().into_bytes();
        let last = tampered.len() - 2;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        assert!(c1
            .decrypt(
                SCOPE_USER_MODELS,
                "u-alice",
                std::str::from_utf8(&tampered).unwrap()
            )
            .is_err());
    }
}
