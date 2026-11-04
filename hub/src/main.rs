use agenct_hub::{app, Config, Hub};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let bind = config.bind;
    let mode = config.runtime.label();
    let hub = Hub::open(config)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("Agenct Hub listening at http://{bind}; runtime={mode}; loopback development only");
    axum::serve(listener, app(hub))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
