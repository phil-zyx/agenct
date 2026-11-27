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
    /// GitLab OAuth team login; `None` when AGENCT_GITLAB_URL is not set.
    pub gitlab: Option<GitLabOAuthConfig>,
    /// Single-owner login with a secret token (no external identity provider).
    pub local_login: bool,
    /// Fixed login token from AGENCT_LOGIN_TOKEN; otherwise one is generated on first start.
    pub local_login_token: Option<String>,
    pub session_ttl_seconds: i64,
    pub llm: LlmConfig,
    /// Whether the default brain model accepts image input (AGENCT_LLM_SUPPORTS_IMAGES).
    pub llm_supports_images: bool,
    /// Optional default image-captioning model (AGENCT_VISION_URL / _API_KEY / _MODEL).
    pub vision_llm: Option<LlmConfig>,
    pub gitlab_webhook_secret: Option<String>,
    pub allowed_gitlab_origins: HashSet<String>,
    pub allow_generic_im_webhook: bool,
    pub allow_private_outbound: bool,
    pub admin_usernames: HashSet<String>,
    /// Optional master key for AEAD encryption at rest (`AGENCT_MASTER_KEY`).
    pub master_key: Option<String>,
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
        // Zero-config default: http://localhost:<port>. Plain HTTP is acceptable there because
        // it is loopback-only (and a secure context in browsers); any other origin must be
        // HTTPS unless AGENCT_ALLOW_HTTP_ORIGIN is set.
        let public_origin = match optional_env_any(&["AGENCT_PUBLIC_ORIGIN"]) {
            Some(raw) => normalize_origin(&raw, allow_http)?,
            None => normalize_origin(&format!("http://localhost:{}", bind.port()), true)?,
        };
        let local_http = public_origin.starts_with("http://");

        let gitlab = match optional_env_any(&["AGENCT_GITLAB_URL", "GITLAB_URL", "GITLAB_BASE_URL"])
        {
            None => None,
            Some(raw_base) => {
                let base_url = normalize_origin(&raw_base, allow_http)?;
                let client_id =
                    required_env_any(&["AGENCT_GITLAB_CLIENT_ID", "GITLAB_OAUTH_CLIENT_ID"])?;
                let client_secret = match std::env::var("AGENCT_GITLAB_CLIENT_SECRET_FILE") {
                    Ok(path) if !path.trim().is_empty() => {
                        std::fs::read_to_string(path.trim())?.trim().to_owned()
                    }
                    _ => required_env_any(&[
                        "AGENCT_GITLAB_CLIENT_SECRET",
                        "GITLAB_OAUTH_CLIENT_SECRET",
                    ])?,
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
                Some(GitLabOAuthConfig {
                    base_url,
                    client_id,
                    client_secret,
                    callback_path,
                })
            }
        };
        // Local owner login: on by default only when there is no GitLab login, so existing
        // GitLab deployments don't silently gain a second way in.
        let local_login = match std::env::var("AGENCT_LOCAL_LOGIN") {
            Ok(v) => matches!(v.trim(), "1" | "true" | "yes" | "on"),
            Err(_) => gitlab.is_none(),
        };
        let local_login_token = match std::env::var("AGENCT_LOGIN_TOKEN") {
            Ok(t) if !t.trim().is_empty() => {
                let t = t.trim().to_owned();
                if t.len() < 16
                    || t.len() > 256
                    || t.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err("AGENCT_LOGIN_TOKEN must be 16..256 visible characters".into());
                }
                Some(t)
            }
            _ => None,
        };
        if gitlab.is_none() && !local_login {
            return Err("no login method: set AGENCT_GITLAB_URL (+ client id/secret) or enable AGENCT_LOCAL_LOGIN=1".into());
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

        let vision_llm = match std::env::var("AGENCT_VISION_URL") {
            Ok(url) if !url.trim().is_empty() => {
                let parsed = reqwest::Url::parse(url.trim())?;
                if !matches!(parsed.scheme(), "http" | "https")
                    || !parsed.path().ends_with("/chat/completions")
                {
                    return Err(
                        "AGENCT_VISION_URL must be an http(s) .../chat/completions URL".into(),
                    );
                }
                Some(LlmConfig::OpenAi {
                    url: url.trim().to_owned(),
                    api_key: std::env::var("AGENCT_VISION_API_KEY").unwrap_or_default(),
                    model: std::env::var("AGENCT_VISION_MODEL")
                        .map_err(|_| "AGENCT_VISION_MODEL is required with AGENCT_VISION_URL")?,
                })
            }
            _ => None,
        };

        let mut allowed_gitlab_origins = HashSet::new();
        if let Some(ref g) = gitlab {
            allowed_gitlab_origins.insert(g.base_url.clone());
        }
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

        let master_key = read_secret_env("AGENCT_MASTER_KEY_FILE", "AGENCT_MASTER_KEY")?;
        if let Some(ref key) = master_key {
            crate::secrets::SecretCipher::from_raw(Some(key))?;
        }

        Ok(Self {
            bind,
            data_dir,
            database_path,
            public_origin,
            local_http,
            gitlab,
            local_login,
            local_login_token,
            session_ttl_seconds,
            llm,
            llm_supports_images: env_flag("AGENCT_LLM_SUPPORTS_IMAGES"),
            vision_llm,
            gitlab_webhook_secret: std::env::var("AGENCT_GITLAB_WEBHOOK_SECRET")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty()),
            allowed_gitlab_origins,
            allow_generic_im_webhook: env_flag("AGENCT_ALLOW_GENERIC_IM_WEBHOOK"),
            allow_private_outbound: env_flag("AGENCT_ALLOW_PRIVATE_OUTBOUND"),
            admin_usernames,
            master_key,
        })
    }

    pub fn authority(&self) -> &str {
        self.public_origin
            .strip_prefix("https://")
            .or_else(|| self.public_origin.strip_prefix("http://"))
            .unwrap_or(&self.public_origin)
    }

    /// Default GitLab instance for the per-user PAT integration: the OAuth instance if
    /// configured, otherwise the first allowlisted origin (or empty).
    pub fn default_gitlab_base(&self) -> String {
        self.gitlab
            .as_ref()
            .map(|g| g.base_url.clone())
            .or_else(|| {
                let mut all: Vec<_> = self.allowed_gitlab_origins.iter().cloned().collect();
                all.sort();
                all.into_iter().next()
            })
            .unwrap_or_default()
    }

    pub fn is_memory_db(&self) -> bool {
        self.database_path.to_str() == Some(":memory:")
    }
}

pub fn read_secret_env(
    file_var: &str,
    env_var: &str,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if let Ok(path) = std::env::var(file_var) {
        let path = path.trim();
        if !path.is_empty() {
            let val = std::fs::read_to_string(path)?.trim().to_owned();
            if !val.is_empty() {
                return Ok(Some(val));
            }
        }
    }
    Ok(std::env::var(env_var)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty()))
}

fn optional_env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
    })
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

async fn validate_outbound_host(
    host: &str,
    port: u16,
    ip_blocked_msg: &'static str,
    dns_fail_msg: &'static str,
    dns_blocked_msg: &'static str,
) -> Result<(), &'static str> {
    let host_trim = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_trim.parse::<IpAddr>() {
        if is_blocked_ip(ip) {
            return Err(ip_blocked_msg);
        }
        return Ok(());
    }
    let addrs = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| dns_fail_msg)?;
    let mut saw_any = false;
    for addr in addrs {
        saw_any = true;
        if is_blocked_ip(addr.ip()) {
            return Err(dns_blocked_msg);
        }
    }
    if !saw_any {
        return Err(dns_fail_msg);
    }
    Ok(())
}

/// Connection-time DNS resolver that rejects any loopback, private, CGNAT, or link-local/metadata IP
/// at the exact moment `reqwest` connects, eliminating DNS Rebinding TOCTOU windows.
pub struct PublicDnsResolver;

impl reqwest::dns::Resolve for PublicDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?
                .collect();
            if addrs.is_empty() {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "DNS resolution returned no addresses",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            if addrs.iter().any(|addr| is_blocked_ip(addr.ip())) {
                return Err(Box::new(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "outbound host resolves to a blocked private/internal IP",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
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
        let port = url.port_or_known_default().unwrap_or(443);
        validate_outbound_host(
            &host,
            port,
            "webhook target IP is blocked",
            "webhook host DNS resolution failed",
            "webhook host resolves to a blocked private/internal IP",
        )
        .await?;
    }
    Ok(trimmed.to_owned())
}

/// Synchronous static validation for a user-configured `/chat/completions` URL
/// (scheme, syntax, path suffix, and literal private/loopback IP checks).
pub fn check_llm_url_static(
    raw: &str,
    allow_private: bool,
) -> Result<(String, String, u16), &'static str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 512 {
        return Err("模型接口地址无效");
    }
    let url = reqwest::Url::parse(trimmed).map_err(|_| "模型接口地址无效")?;
    let scheme_ok = url.scheme() == "https" || (allow_private && url.scheme() == "http");
    if !scheme_ok
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err("模型接口地址必须是干净的 https:// URL");
    }
    if !url.path().ends_with("/chat/completions") {
        return Err("模型接口地址必须以 /chat/completions 结尾（OpenAI 兼容格式）");
    }
    let host = url.host_str().ok_or("模型接口地址缺少主机名")?.to_owned();
    let port = url.port_or_known_default().unwrap_or(443);
    if !allow_private {
        let host_trim = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host_trim.parse::<IpAddr>() {
            if is_blocked_ip(ip) {
                return Err("不允许指向内网/本机地址的模型接口");
            }
        }
    }
    Ok((trimmed.to_owned(), host, port))
}

/// Validates a user-configured OpenAI-compatible `/chat/completions` URL against SSRF:
/// HTTPS + public IP only, unless the operator set AGENCT_ALLOW_PRIVATE_OUTBOUND.
pub async fn validate_llm_url(raw: &str, allow_private: bool) -> Result<String, &'static str> {
    let (trimmed, host, port) = check_llm_url_static(raw, allow_private)?;
    if !allow_private {
        let host_trim = host.trim_start_matches('[').trim_end_matches(']');
        if host_trim.parse::<IpAddr>().is_err() {
            validate_outbound_host(
                &host,
                port,
                "不允许指向内网/本机地址的模型接口",
                "模型接口域名解析失败",
                "模型接口域名解析到了内网/本机地址",
            )
            .await?;
        }
    }
    Ok(trimmed)
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

/// Keeps both the leading context (`head_bytes`) and the newest/trailing lines (`tail_bytes`).
pub fn truncate_head_tail_bytes(s: &str, head_bytes: usize, tail_bytes: usize) -> String {
    if s.len() <= head_bytes.saturating_add(tail_bytes).saturating_add(16) {
        return s.to_owned();
    }
    let mut head_end = head_bytes.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(tail_bytes);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if head_end >= tail_start {
        return s.to_owned();
    }
    format!(
        "{}\n…(省略中间)…\n{}",
        s[..head_end].trim_end(),
        s[tail_start..].trim_start()
    )
}

#[cfg(test)]
mod tests {
    use super::truncate_head_tail_bytes;

    #[test]
    fn truncate_head_tail_preserves_utf8_boundaries_and_both_ends() {
        let short = "短消息测试";
        assert_eq!(truncate_head_tail_bytes(short, 100, 100), short);

        let middle = "中".repeat(100);
        let long = format!("头部结论：准备发布。\n{middle}\n尾部下一步：继续啃 P0-1 崩溃修复。");
        let out = truncate_head_tail_bytes(&long, 25, 35);
        assert!(out.starts_with("头部结论"));
        assert!(out.contains("…(省略中间)…"));
        assert!(out.ends_with("继续啃 P0-1 崩溃修复。"));
    }

    #[tokio::test]
    async fn public_dns_resolver_blocks_localhost() {
        let client = reqwest::Client::builder()
            .no_proxy()
            .dns_resolver(std::sync::Arc::new(super::PublicDnsResolver))
            .build()
            .unwrap();
        let err = client
            .get("http://localhost:9/")
            .send()
            .await
            .expect_err("localhost must be blocked at connect-time DNS resolution");
        let chain = format!("{err:?}");
        assert!(
            chain.contains("blocked private/internal IP") || err.is_connect(),
            "unexpected error: {chain}"
        );
    }
}
