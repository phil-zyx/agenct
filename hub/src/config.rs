//! Unified Hub configuration and security validation helpers (SSRF & timing safety).
use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
};

#[derive(Clone, Debug)]
pub struct GitLabOAuthConfig {
    pub base_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub callback_path: String,
    pub session_ttl_seconds: i64,
}

#[derive(Clone, Debug)]
pub enum LlmConfig {
    OpenAi {
        url: String,
        api_key: String,
        model: String,
    },
    Stub,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub database_path: PathBuf,
    pub public_origin: String,
    pub local_http: bool,
    pub gitlab: GitLabOAuthConfig,
    pub llm: LlmConfig,
    pub gitlab_webhook_secret: Option<String>,
    pub allowed_gitlab_origins: HashSet<String>,
    pub allow_generic_im_webhook: bool,
    pub allow_private_outbound: bool,
    pub admin_usernames: HashSet<String>,
}

impl Config {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let bind: SocketAddr = std::env::var("AGENCT_BIND")
            .unwrap_or_else(|_| "127.0.0.1:8080".into())
            .parse()?;

        let data_dir =
            PathBuf::from(std::env::var("AGENCT_DATA_DIR").unwrap_or_else(|_| "./data".into()));
        let database_path = std::env::var("AGENCT_DATABASE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| data_dir.join("agenct.sqlite3"));

        let allow_http = env_flag("AGENCT_ALLOW_HTTP_ORIGIN");
        let public_origin = normalize_origin(
            &std::env::var("AGENCT_PUBLIC_ORIGIN").unwrap_or_else(|_| format!("http://{bind}")),
            allow_http,
        )?;
        let local_http = public_origin.starts_with("http://");

        let gitlab_base = normalize_origin(
            &required_env_any(&["AGENCT_GITLAB_URL", "GITLAB_URL", "GITLAB_BASE_URL"])?,
            allow_http,
        )?;
        let client_id = required_env_any(&["AGENCT_GITLAB_CLIENT_ID", "GITLAB_OAUTH_CLIENT_ID"])?;
        let client_secret = match std::env::var("AGENCT_GITLAB_CLIENT_SECRET_FILE") {
            Ok(path) if !path.trim().is_empty() => {
                std::fs::read_to_string(path.trim())?.trim().to_owned()
            }
            _ => required_env_any(&["AGENCT_GITLAB_CLIENT_SECRET", "GITLAB_OAUTH_CLIENT_SECRET"])?,
        };
        if client_secret.is_empty() || client_secret.len() > 4096 {
            return Err("invalid AGENCT_GITLAB_CLIENT_SECRET".into());
        }
        let callback_path = std::env::var("AGENCT_GITLAB_CALLBACK_PATH")
            .unwrap_or_else(|_| "/auth/callback".into());
        if !matches!(
            callback_path.as_str(),
            "/auth/callback" | "/identity/v1/auth/oidc/gitlab/callback"
        ) {
            return Err("AGENCT_GITLAB_CALLBACK_PATH must be /auth/callback or /identity/v1/auth/oidc/gitlab/callback".into());
        }
        let session_ttl_seconds: i64 = std::env::var("AGENCT_SESSION_TTL_SECONDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(28_800);
        if !(300..=86_400).contains(&session_ttl_seconds) {
            return Err("AGENCT_SESSION_TTL_SECONDS must be between 300 and 86400".into());
        }

        let llm = if env_flag("AGENCT_LLM_STUB") {
            LlmConfig::Stub
        } else {
            let url = std::env::var("AGENCT_LLM_URL")
                .unwrap_or_else(|_| "https://api.deepseek.com/v1/chat/completions".into());
            let parsed = reqwest::Url::parse(&url)?;
            if !matches!(parsed.scheme(), "http" | "https")
                || !parsed.path().ends_with("/chat/completions")
            {
                return Err("AGENCT_LLM_URL must be an http(s) .../chat/completions URL".into());
            }
            let api_key = std::env::var("AGENCT_LLM_API_KEY")
                .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
                .or_else(|_| std::env::var("OPENAI_API_KEY"))
                .unwrap_or_else(|_| "local-llm".into());
            let model = std::env::var("AGENCT_LLM_MODEL")
                .or_else(|_| std::env::var("OPENAI_MODEL"))
                .unwrap_or_else(|_| "deepseek-chat".into());
            LlmConfig::OpenAi {
                url,
                api_key,
                model,
            }
        };

        let mut allowed_gitlab_origins = HashSet::new();
        allowed_gitlab_origins.insert(gitlab_base.clone());
        if let Ok(extra) = std::env::var("AGENCT_ALLOWED_GITLAB_ORIGINS") {
            for item in extra.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                allowed_gitlab_origins.insert(normalize_origin(item, allow_http)?);
            }
        }

        let admin_usernames = std::env::var("AGENCT_ADMINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            bind,
            data_dir,
            database_path,
            public_origin,
            local_http,
            gitlab: GitLabOAuthConfig {
                base_url: gitlab_base,
                client_id,
                client_secret,
                callback_path,
                session_ttl_seconds,
            },
            llm,
            gitlab_webhook_secret: std::env::var("AGENCT_GITLAB_WEBHOOK_SECRET")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty()),
            allowed_gitlab_origins,
            allow_generic_im_webhook: env_flag("AGENCT_ALLOW_GENERIC_IM_WEBHOOK"),
            allow_private_outbound: env_flag("AGENCT_ALLOW_PRIVATE_OUTBOUND"),
            admin_usernames,
        })
    }

    pub fn authority(&self) -> &str {
        self.public_origin
            .strip_prefix("https://")
            .or_else(|| self.public_origin.strip_prefix("http://"))
            .unwrap_or(&self.public_origin)
    }

    pub fn is_memory_db(&self) -> bool {
        self.database_path.to_str() == Some(":memory:")
    }
}

fn required_env_any(names: &[&str]) -> Result<String, Box<dyn std::error::Error>> {
    for name in names {
        if let Ok(val) = std::env::var(name) {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                return Ok(trimmed.to_owned());
            }
        }
    }
    Err(format!("missing required environment variable {}", names[0]).into())
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes")
    )
}

pub fn normalize_origin(raw: &str, allow_http: bool) -> Result<String, &'static str> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 256 {
        return Err("invalid origin URL");
    }
    let url = reqwest::Url::parse(trimmed).map_err(|_| "invalid origin URL")?;
    let ok_scheme = url.scheme() == "https" || (allow_http && url.scheme() == "http");
    if !ok_scheme
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("origin must be a clean https:// URL (or http:// when explicitly allowed)");
    }
    Ok(url.origin().ascii_serialization())
}

pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT
                || (o[0] == 169 && o[1] == 254) // metadata / link-local
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_blocked_ip(IpAddr::V4(v4)))
        }
    }
}

/// Validates an IM webhook URL against SSRF (official HTTPS domains by default + DNS IP checks).
pub async fn validate_im_webhook_url(
    provider: &str,
    raw: &str,
    allow_generic: bool,
    allow_private: bool,
) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err("invalid webhook URL");
    }
    let url = reqwest::Url::parse(trimmed).map_err(|_| "invalid webhook URL")?;
    let scheme_ok = url.scheme() == "https" || (allow_private && url.scheme() == "http");
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("webhook URL must be a clean https:// URL");
    }
    let host = url
        .host_str()
        .ok_or("webhook host required")?
        .to_ascii_lowercase();

    let domain_allowed = match provider {
        "feishu" => host == "open.feishu.cn" || host == "open.larksuite.com",
        "wecom" => host == "qyapi.weixin.qq.com",
        "dingtalk" => host == "oapi.dingtalk.com",
        "generic" => allow_generic,
        _ => return Err("unsupported IM provider"),
    };
    if !domain_allowed && !allow_private {
        return Err("webhook host does not match provider allowlist");
    }

    if !allow_private {
        if let Ok(ip) = host.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err("webhook target IP is blocked");
            }
        } else {
            let port = url.port_or_known_default().unwrap_or(443);
            let addrs = tokio::net::lookup_host((host.as_str(), port))
                .await
                .map_err(|_| "webhook host DNS resolution failed")?;
            let mut saw_any = false;
            for addr in addrs {
                saw_any = true;
                if is_blocked_ip(addr.ip()) {
                    return Err("webhook host resolves to a blocked private/internal IP");
                }
            }
            if !saw_any {
                return Err("webhook host resolved to no addresses");
            }
        }
    }
    Ok(trimmed.to_owned())
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn truncate_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}
