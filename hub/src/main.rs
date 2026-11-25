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
    if std::env::args().nth(1).as_deref() == Some("reencrypt-secrets") {
        let next = agenct_hub::secrets::SecretCipher::from_raw(config.master_key.as_deref())?;
        if !next.is_enabled() {
            return Err("AGENCT_MASTER_KEY is required to re-encrypt secrets".into());
        }
        let old_raw = agenct_hub::config::read_secret_env(
            "AGENCT_MASTER_KEY_OLD_FILE",
            "AGENCT_MASTER_KEY_OLD",
        )?;
        let old = match old_raw.as_deref() {
            Some(k) => Some(agenct_hub::secrets::SecretCipher::from_raw(Some(k))?),
            None => None,
        };
        let db = agenct_hub::db::Db::open(&config.database_path)?;
        let count = db
            .write_blocking(|conn| agenct_hub::secrets::reencrypt_db(conn, &next, old.as_ref()))
            .map_err(|e| e.1)?;
        println!("Re-encrypted {count} secret record(s).");
        return Ok(());
    }
    let bind = config.bind;
    let origin = config.public_origin.clone();
    let hub = Hub::open(config)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!(
        "Agenct Hub v2 listening on http://{bind} (origin={origin}, llm={}, secrets={})",
        hub.llm.label(),
        if hub.secrets.is_enabled() {
            "encrypted"
        } else {
            "plaintext"
        }
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
