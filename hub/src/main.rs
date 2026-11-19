use agenct_hub::{app, config::Config, Hub};
use std::future::IntoFuture;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    let bind = config.bind;
    let origin = config.public_origin.clone();
    let hub = Hub::open(config)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!(
        "Agenct Hub v2 listening on http://{bind} (origin={origin}, llm={})",
        hub.llm.label()
    );

    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let stopped = shutdown.clone();
    let mut scheduler = Box::pin(hub.run_scheduler());
    let mut server = Box::pin(
        axum::serve(listener, app(hub.clone()))
            .with_graceful_shutdown(async move { stopped.notified().await })
            .into_future(),
    );

    tokio::select! {
        res = &mut server => res?,
        () = &mut scheduler => return Err("scheduler exited unexpectedly".into()),
        _ = tokio::signal::ctrl_c() => {
            drop(scheduler);
            shutdown.notify_one();
            server.await?;
        }
    }
    Ok(())
}
