//! Per-user model settings: each user may bring their own brain model (optionally
//! image-capable) and a separate image-captioning ("vision") model. Anything not configured
//! falls back to the server defaults from the environment.
use crate::{
    auth::AuthUser,
    config::{now_unix, validate_llm_url, LlmConfig},
    invalid,
    llm::LlmClient,
    secrets::SCOPE_USER_MODELS,
    Hub, Result,
};
use axum::{extract::State, Json};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModelEndpoint {
    pub url: String,
    #[serde(default)]
    pub api_key: String,
    pub model: String,
    /// Whether this model accepts OpenAI-style `image_url` content parts.
    #[serde(default)]
    pub supports_images: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UserModels {
    #[serde(default)]
    pub brain: Option<ModelEndpoint>,
    #[serde(default)]
    pub vision: Option<ModelEndpoint>,
}

/// The models actually used for one user right now.
pub struct ResolvedModels {
    pub brain: LlmClient,
    pub brain_sees_images: bool,
    pub vision: Option<LlmClient>,
}

fn client_for(hub: &Hub, ep: &ModelEndpoint) -> LlmClient {
    LlmClient::new(
        LlmConfig::OpenAi {
            url: ep.url.clone(),
            api_key: ep.api_key.clone(),
            model: ep.model.clone(),
        },
        hub.http.clone(),
    )
}

pub async fn load(hub: &Hub, user_id: &str) -> Result<UserModels> {
    let uid = user_id.to_owned();
    let secrets = hub.secrets.clone();
    hub.db
        .read(move |conn| {
            let raw: Option<String> = conn
                .query_row(
                    "SELECT config_json FROM user_models WHERE user_id = ?1",
                    [&uid],
                    |r| r.get(0),
                )
                .optional()?;
            secrets.decrypt_json(SCOPE_USER_MODELS, &uid, raw)
        })
        .await
}

/// Resolves the effective brain / vision models for a user (user settings over server defaults).
pub async fn resolve(hub: &Hub, user_id: &str) -> Result<ResolvedModels> {
    let cfg = load(hub, user_id).await?;
    let (brain, brain_sees_images) = match &cfg.brain {
        Some(ep) => (client_for(hub, ep), ep.supports_images),
        None => (hub.llm.clone(), hub.config.llm_supports_images),
    };
    let vision = match &cfg.vision {
        Some(ep) => Some(client_for(hub, ep)),
        None => hub
            .config
            .vision_llm
            .clone()
            .map(|c| LlmClient::new(c, hub.http.clone())),
    };
    Ok(ResolvedModels {
        brain,
        brain_sees_images,
        vision,
    })
}

fn mask_key(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "****".into();
    }
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("****{tail}")
}

fn endpoint_view(ep: &Option<ModelEndpoint>) -> Value {
    match ep {
        None => Value::Null,
        Some(ep) => json!({
            "url": ep.url,
            "model": ep.model,
            "supports_images": ep.supports_images,
            "key_preview": if ep.api_key.is_empty() { String::new() } else { mask_key(&ep.api_key) },
        }),
    }
}

fn settings_view(hub: &Hub, cfg: &UserModels) -> Value {
    json!({
        "brain": endpoint_view(&cfg.brain),
        "vision": endpoint_view(&cfg.vision),
        "server_default": {
            "brain_model": hub.llm.model_name(),
            "brain_supports_images": hub.config.llm_supports_images,
            "vision_model": hub.config.vision_llm.as_ref().map(|c| match c {
                LlmConfig::OpenAi { model, .. } => model.clone(),
                LlmConfig::Stub => "stub".into(),
            }),
        },
    })
}

pub async fn get_settings(State(hub): State<Hub>, AuthUser(user): AuthUser) -> Result<Json<Value>> {
    let cfg = load(&hub, &user.id).await?;
    Ok(Json(settings_view(&hub, &cfg)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointInput {
    pub url: String,
    pub model: String,
    /// Empty / omitted keeps the previously saved key for this slot.
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub supports_images: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveSettingsInput {
    #[serde(default)]
    pub brain: Option<EndpointInput>,
    #[serde(default)]
    pub vision: Option<EndpointInput>,
    #[serde(default)]
    pub clear_brain: bool,
    #[serde(default)]
    pub clear_vision: bool,
}

async fn validated_endpoint(
    hub: &Hub,
    input: &EndpointInput,
    previous: Option<&ModelEndpoint>,
) -> Result<ModelEndpoint> {
    let url = validate_llm_url(&input.url, hub.config.allow_private_outbound)
        .await
        .map_err(invalid)?;
    let model = input.model.trim();
    if model.is_empty() || model.len() > 128 || model.chars().any(char::is_control) {
        return Err(invalid("模型名必须是 1..128 个字符"));
    }
    let api_key = match input.api_key.as_deref().map(str::trim) {
        Some(k) if !k.is_empty() => {
            if k.len() > 512 || k.chars().any(|c| c.is_control() || c.is_whitespace()) {
                return Err(invalid("API Key 格式无效"));
            }
            k.to_owned()
        }
        _ => previous.map(|p| p.api_key.clone()).unwrap_or_default(),
    };
    if api_key.is_empty() {
        return Err(invalid("请填写 API Key"));
    }
    Ok(ModelEndpoint {
        url,
        api_key,
        model: model.to_owned(),
        supports_images: input.supports_images,
    })
}

pub async fn save_settings(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
    Json(input): Json<SaveSettingsInput>,
) -> Result<Json<Value>> {
    let mut cfg = load(&hub, &user.id).await?;
    if input.clear_brain {
        cfg.brain = None;
    } else if let Some(ref b) = input.brain {
        cfg.brain = Some(validated_endpoint(&hub, b, cfg.brain.as_ref()).await?);
    }
    if input.clear_vision {
        cfg.vision = None;
    } else if let Some(ref v) = input.vision {
        cfg.vision = Some(validated_endpoint(&hub, v, cfg.vision.as_ref()).await?);
    }

    let uid = user.id.clone();
    let encoded = hub
        .secrets
        .encrypt_json(SCOPE_USER_MODELS, &user.id, &cfg)?;
    let summary = format!(
        "brain={} vision={}",
        cfg.brain.as_ref().map_or("默认", |b| b.model.as_str()),
        cfg.vision.as_ref().map_or("默认", |v| v.model.as_str())
    );
    hub.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO user_models(user_id, config_json, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(user_id) DO UPDATE SET config_json=excluded.config_json, updated_at=excluded.updated_at",
                params![uid, encoded, now_unix()],
            )?;
            crate::db::record_audit(conn, &uid, "model_settings", "ok", "Updated model settings", &summary)?;
            Ok(())
        })
        .await?;
    Ok(Json(settings_view(&hub, &cfg)))
}

/// Sends a tiny prompt to the effective brain (and vision model, if any) and reports results.
pub async fn test_settings(
    State(hub): State<Hub>,
    AuthUser(user): AuthUser,
) -> Result<Json<Value>> {
    let models = resolve(&hub, &user.id).await?;
    let probe = |client: LlmClient| async move {
        match client
            .complete("你是连通性测试。只回复 OK。", &[], "ping", false)
            .await
        {
            Ok(_) => json!({"ok": true, "model": client.model_name()}),
            Err(e) => json!({"ok": false, "model": client.model_name(), "error": e.1}),
        }
    };
    let (brain, vision) = tokio::join!(probe(models.brain), async {
        match models.vision {
            Some(v) => probe(v).await,
            None => Value::Null,
        }
    });
    Ok(Json(json!({"brain": brain, "vision": vision})))
}
