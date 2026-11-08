use agenct_hub::{app, Config, Hub};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let bind = config.bind;
    let mode = config.runtime.label();
    let mut hub = Hub::open(config)?;
    if let Some(path) = std::env::var_os("AGENCT_OIDC_CONFIG_FILE") {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(131073)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 131072 {
            return Err("OIDC configuration exceeds 128 KiB".into());
        }
        hub = hub.with_oidc(serde_json::from_slice(&bytes)?)?;
        eprintln!("Authentication: pinned OIDC access tokens; development tokens disabled");
    }
    if let Some(path) = std::env::var_os("AGENCT_BROWSER_OIDC_CONFIG_FILE") {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(131073)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 131072 {
            return Err("browser OIDC config exceeds 128 KiB".into());
        }
        hub = hub.with_browser_auth(serde_json::from_slice(&bytes)?)?;
        eprintln!("Authentication: opt-in OIDC browser sessions; bearer tokens disabled");
    }
    if let Ok(value) = std::env::var("AGENCT_MEMBER_ADMINS_JSON") {
        hub = hub.with_member_admins(serde_json::from_str(&value)?)?;
    }
    if let Some(path) = std::env::var_os("AGENCT_WORKER_SCOPES_FILE") {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(131073)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 131072 {
            return Err("worker scopes exceed 128 KiB".into());
        }
        hub = hub.with_worker_scopes(serde_json::from_slice(&bytes)?)?;
        eprintln!("Worker queue admission configured; execution and publication remain disabled");
    }
    if let Some(path) = std::env::var_os("AGENCT_PRIVATE_CONFIG") {
        hub = hub.with_private_config(path.into())?;
    }
    if let Some(path) = std::env::var_os("AGENCT_REPOSITORIES_FILE") {
        let gate = std::env::var_os("AGENCT_REPOSITORY_GATE_FILE")
            .ok_or("AGENCT_REPOSITORY_GATE_FILE required for read-only repository access")?;
        hub = hub.with_repositories(path.into(), gate.into())?;
    }
    if let Some(path) = std::env::var_os("AGENCT_TENANTS_FILE") {
        use std::io::Read;
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(131073)
            .read_to_end(&mut bytes)?;
        if bytes.len() > 131072 {
            return Err("tenant mapping exceeds 128 KiB".into());
        }
        let assignments: agenct_hub::tenants::Config = serde_json::from_slice(&bytes)?;
        hub = hub.with_tenants(assignments)?;
        eprintln!("Organization isolation enabled; cross-organization delegation disabled");
    }
    // On by default: the owner still reviews, edits and explicitly delivers.
    let auto_draft = std::env::var("AGENCT_AUTO_DRAFT").map_or(true, |v| v != "0");
    hub = hub.with_auto_draft(auto_draft);
    if !auto_draft {
        eprintln!("Automatic drafting disabled (AGENCT_AUTO_DRAFT=0)");
    }
    hub.validate_startup()?;
    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "Cannot start: {bind} is already in use by another program. Anything answering \
                 there is not this Hub instance. Stop that program or choose another port \
                 (AGENCT_BIND, or `python3 scripts/private-staging.py set-port`)."
            );
            std::process::exit(1);
        }
        Err(error) => return Err(error.into()),
    };
    eprintln!("Agenct Hub listening at http://{bind}; runtime={mode}; development service");
    axum::serve(listener, app(hub))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
