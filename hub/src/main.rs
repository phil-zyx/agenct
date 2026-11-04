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
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("Agenct Hub listening at http://{bind}; runtime={mode}; loopback development only");
    axum::serve(listener, app(hub))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
