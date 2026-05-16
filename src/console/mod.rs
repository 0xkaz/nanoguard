//! Nanoguard Console — Web Configuration UI
//!
//! Phase 1 implementation: read-only console + local-password auth +
//! own-token self-service.
//!
//! See `docs/design/web-config-ui.md` for the full design.

use axum::{
    routing::{delete, get, post, put},
    Router,
};
use std::net::SocketAddr;
use std::sync::Arc;

pub mod assets;
pub mod auth;
pub mod db;
pub mod handlers;

use crate::client_auth::store as token_store;
use crate::config::Config;

/// Shared state for the console server.
pub struct ConsoleState {
    pub config: Config,
    pub db: db::ConsoleDb,
    pub secure_cookie: bool,
}

/// Run the console server. This function blocks until shutdown.
pub async fn run(config: Config) -> anyhow::Result<()> {
    if config.console.session_secret.is_empty() {
        anyhow::bail!(
            "console.session_secret is required. Set CONSOLE_SESSION_SECRET or add it to nanoguard.toml"
        );
    }

    let db_path = &config.budget.db_path;
    let db = db::ConsoleDb::open(db_path)?;

    // Ensure the client_tokens table exists too (we may write tokens).
    db.with_conn(|conn| {
        token_store::migrate(conn)?;
        Ok(())
    })?;

    // Bootstrap admin if configured and no users exist.
    if let Some(ref bootstrap) = config.console.auth.local.bootstrap_admin {
        if let Ok(pw) = std::env::var(&bootstrap.password_env) {
            let hash = auth::hash_password(&pw)?;
            let created = db.with_conn(|conn| {
                db::maybe_bootstrap_admin(conn, &bootstrap.username, &hash)
            })?;
            if created {
                tracing::info!(
                    "Bootstrap admin '{}' provisioned. Clear ${} from the environment.",
                    bootstrap.username,
                    bootstrap.password_env
                );
            }
            // NOTE: We cannot reliably unset the env var from the process
            // image in a portable way. The operator must clear it from the
            // shell / service definition after first start.
        }
    }

    // Determine if cookies should be marked Secure.
    // If listening on loopback, no. Otherwise, yes.
    let listen = config.console.listen.parse::<SocketAddr>()?;
    let secure_cookie = !listen.ip().is_loopback();

    let state = Arc::new(ConsoleState {
        config: config.clone(),
        db,
        secure_cookie,
    });

    // Spawn a background task to prune expired sessions every 5 minutes.
    let prune_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            match prune_state.db.with_conn(db::prune_expired_sessions) {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("Pruned {} expired sessions", n);
                    }
                }
                Err(e) => tracing::warn!("Session prune failed: {}", e),
            }
        }
    });

    let app = Router::new()
        .route("/", get(handlers::index))
        .route("/styles.css", get(handlers::styles))
        .route("/app.js", get(handlers::app_js))
        .route("/api/login", post(handlers::api_login))
        .route("/api/logout", post(handlers::api_logout))
        .route("/api/me", get(handlers::api_me))
        .route("/api/tokens", get(handlers::api_list_tokens).post(handlers::api_create_token))
        .route("/api/tokens/:id", delete(handlers::api_revoke_token))
        .route("/api/audit", get(handlers::api_audit))
        .route("/api/budget", get(handlers::api_budget))
        .route("/api/config", get(handlers::api_config))
        .route("/api/users", get(handlers::api_list_users).post(handlers::api_create_user))
        .route("/api/users/:id", put(handlers::api_update_user))
        .with_state(state);

    tracing::info!("nanoguard-console listening on http://{}", listen);
    if secure_cookie {
        tracing::info!("Cookies are marked Secure (non-loopback listener)");
    } else {
        tracing::info!("Cookies are NOT marked Secure (loopback listener)");
    }

    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("nanoguard-console stopped gracefully");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let sigterm = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("received Ctrl+C, shutting down..."); },
        _ = sigterm => { tracing::info!("received SIGTERM, shutting down..."); },
    }
}
