//! SSRF-hardened IM integration (Feishu / WeCom / DingTalk) with per-user inbound webhook token.
use crate::{
    auth::{random_token, sha256_hex, AuthUser},
    config::{constant_time_eq, now_unix, truncate_bytes, validate_im_webhook_url},
    denied, invalid,
    secrets::SCOPE_IM,
    Error, Hub, Result,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImConfig {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub webhook_url: String,
    #[serde(default)]
    pub inbound_token_hash: String,
    #[serde(default = "default_true")]
    pub notify_delegation: bool,
    #[serde(default = "default_true")]
    pub notify_delivery: bool,
}

fn default_provider() -> String {
    "feishu".into()
}
fn default_true() -> bool {
    true
}

impl Default for ImConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            webhook_url: String::new(),
            inbound_token_hash: String::new(),
            notify_delegation: true,
            notify_delivery: true,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveImInput {
    #[serde(default = "default_provider")]
    pub provider: String,
    #[serde(default)]
    pub webhook_url: Option<String>,
    #[serde(default)]
    pub clear_webhook: bool,
    #[serde(default)]
    pub rotate_inbound_token: bool,
    #[serde(default = "default_true")]
    pub notify_delegation: bool,
    #[serde(default = "default_true")]
    pub notify_delivery: bool,
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

pub async fn get_config(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let cfg = load_im_config(&hub, &user.id).await?;
    Ok(Json(json!({
        "provider": cfg.provider,
        "configured": !cfg.webhook_url.is_empty(),
        "inbound_configured": !cfg.inbound_token_hash.is_empty(),
        "notify_delegation": cfg.notify_delegation,
        "notify_delivery": cfg.notify_delivery,
    })))
}

pub async fn save_config(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<SaveImInput>,
) -> Result<Json<Value>> {
    let provider = input.provider.trim().to_ascii_lowercase();
    if !matches!(
        provider.as_str(),
        "feishu" | "wecom" | "dingtalk" | "generic"
    ) {
        return Err(invalid(
            "provider must be feishu, wecom, dingtalk, or generic",
        ));
    }

    let mut cfg = load_im_config(&hub, &user.id).await?;
    cfg.provider = provider.clone();
    if input.clear_webhook {
        cfg.webhook_url.clear();
    } else if let Some(raw) = input
        .webhook_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let validated = validate_im_webhook_url(
            &provider,
            raw,
            hub.config.allow_generic_im_webhook,
            hub.config.allow_private_outbound,
        )
        .await
        .map_err(invalid)?;
        cfg.webhook_url = validated;
    }

    let mut new_inbound_token: Option<String> = None;
    if input.rotate_inbound_token || cfg.inbound_token_hash.is_empty() {
        let tok = format!("im_{}", random_token());
        cfg.inbound_token_hash = sha256_hex(&tok);
        new_inbound_token = Some(tok);
    }
    cfg.notify_delegation = input.notify_delegation;
    cfg.notify_delivery = input.notify_delivery;

    let uid = user.id.clone();
    let encoded = hub.secrets.encrypt_json(SCOPE_IM, &user.id, &cfg)?;
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO integrations(user_id, kind, config_json, updated_at)
                 VALUES (?1, 'im', ?2, ?3)
                 ON CONFLICT(user_id, kind) DO UPDATE SET config_json=excluded.config_json, updated_at=excluded.updated_at",
                params![uid, encoded, now_unix()],
            )?;
            Ok(())
        })
        .await?;

    Ok(Json(json!({
        "ok": true,
        "provider": cfg.provider,
        "configured": !cfg.webhook_url.is_empty(),
        "inbound_token": new_inbound_token,
        "notify_delegation": cfg.notify_delegation,
        "notify_delivery": cfg.notify_delivery,
    })))
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

pub async fn maybe_notify(hub: &Hub, user_id: &str, event_kind: &str, title: &str, content: &str) {
    let Ok(cfg) = load_im_config(hub, user_id).await else {
        return;
    };
    if cfg.webhook_url.is_empty() {
        return;
    }
    let enabled = match event_kind {
        "delegation" => cfg.notify_delegation,
        "delivery" => cfg.notify_delivery,
        _ => true,
    };
    if !enabled {
        return;
    }
    // Re-validate before outbound call to defend against DNS rebinding.
    if validate_im_webhook_url(
        &cfg.provider,
        &cfg.webhook_url,
        hub.config.allow_generic_im_webhook,
        hub.config.allow_private_outbound,
    )
    .await
    .is_err()
    {
        return;
    }
    let payload = build_card_payload(&cfg.provider, title, content);
    let _ = hub
        .user_http
        .post(&cfg.webhook_url)
        .json(&payload)
        .send()
        .await;
}

pub async fn test_webhook(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let cfg = load_im_config(&hub, &user.id).await?;
    if cfg.webhook_url.is_empty() {
        return Err(invalid("configure IM webhook_url first"));
    }
    validate_im_webhook_url(
        &cfg.provider,
        &cfg.webhook_url,
        hub.config.allow_generic_im_webhook,
        hub.config.allow_private_outbound,
    )
    .await
    .map_err(invalid)?;

    let payload = build_card_payload(
        &cfg.provider,
        "Agenct 通知通道测试",
        &format!("账号 `{}` 的实时通知已连通。", user.username),
    );
    let resp = hub
        .user_http
        .post(&cfg.webhook_url)
        .json(&payload)
        .send()
        .await
        .map_err(|_| Error(StatusCode::BAD_GATEWAY, "IM webhook unreachable"))?;
    if !resp.status().is_success() {
        return Err(Error(
            StatusCode::BAD_GATEWAY,
            "IM webhook returned non-2xx",
        ));
    }
    Ok(Json(json!({"ok": true, "provider": cfg.provider})))
}

fn extract_inbound_text(payload: &Value) -> Option<String> {
    if let Some(s) = payload["text"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(s) = payload["text"]["content"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(s) = payload["content"].as_str() {
        return Some(s.trim().to_owned());
    }
    if let Some(raw) = payload["event"]["message"]["content"].as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            if let Some(t) = parsed["text"].as_str() {
                return Some(t.trim().to_owned());
            }
        }
        return Some(raw.trim().to_owned());
    }
    None
}

/// Inbound IM webhook authenticated by per-user `X-Agenct-IM-Token` header.
pub async fn inbound_webhook(
    State(hub): State<Hub>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>> {
    if let Some(challenge) = payload.get("challenge").and_then(Value::as_str) {
        return Ok(Json(json!({"challenge": challenge})));
    }
    let token = headers
        .get("x-agenct-im-token")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| s.starts_with("im_") && s.len() >= 35)
        .ok_or_else(denied)?;
    let hash = sha256_hex(token);

    let secrets = hub.secrets.clone();
    let owner_id: Option<String> = hub
        .db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT i.user_id, i.config_json
                 FROM integrations i JOIN users u ON u.id = i.user_id
                 WHERE i.kind = 'im' AND u.status = 'active'",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows.flatten() {
                if let Ok(cfg) = secrets.decrypt_json::<ImConfig>(SCOPE_IM, &row.0, Some(row.1)) {
                    if !cfg.inbound_token_hash.is_empty()
                        && constant_time_eq(cfg.inbound_token_hash.as_bytes(), hash.as_bytes())
                    {
                        return Ok(Some(row.0));
                    }
                }
            }
            Ok(None)
        })
        .await?;
    let user_id = owner_id.ok_or_else(denied)?;

    let text = extract_inbound_text(&payload).ok_or_else(|| invalid("missing IM text"))?;
    if text.is_empty() || text.len() > 4000 {
        return Err(invalid("IM text must be 1..4000 bytes"));
    }

    let reply = crate::brain::run_turn(&hub, &user_id, &text, None, None).await?;
    Ok(Json(json!({
        "ok": true,
        "reply": reply["reply"],
    })))
}
