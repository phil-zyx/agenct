use agenct_hub::{app, config::Config, Hub};
use std::future::IntoFuture;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env()?;
    if std::env::args().nth(1).as_deref() == Some("reset-login-token") {
        // Requires the Hub to be stopped (the database is exclusively locked while running).
        if !config.local_login {
            return Err("local login is disabled (set AGENCT_LOCAL_LOGIN=1)".into());
        }
        let db = agenct_hub::db::Db::open(&config.database_path)?;
        let (token, saved) = db
            .write_blocking(|conn| agenct_hub::auth::rotate_local_token(conn, &config))
            .map_err(|e| e.1)?;
        println!("New local login token: {token}");
        if let Some(path) = saved {
            println!("Saved to {}", path.display());
        }
        return Ok(());
    }
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
