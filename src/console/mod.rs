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
pub mod audit;
pub mod auth;
pub mod db;
pub mod edit;
pub mod handlers;
pub mod reload;

use crate::client_auth::store as token_store;
use crate::config::Config;

/// Shared state for the console server.
pub struct ConsoleState {
    pub config: Config,
    pub db: db::ConsoleDb,
    pub secure_cookie: bool,
    pub audit_log: Option<audit::ConsoleAuditLog>,
}

/// Run the console server. This function blocks until shutdown.
///
/// `bootstrap_password` is an optional plaintext password wrapped in
/// [`Zeroizing`] so it is cleared from memory after hashing. It must be
/// read from the environment **before** the tokio runtime starts so it
/// does not remain visible in `/proc/<pid>/environ` for the process
/// lifetime.
pub async fn run(
    mut config: Config,
    bootstrap_password: Option<zeroize::Zeroizing<String>>,
) -> anyhow::Result<()> {
    if config.console.session_secret.is_empty() {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        let secret = hex::encode(bytes);

        tracing::warn!(
            "[console] session_secret is empty; generated ephemeral secret. \
             Set CONSOLE_SESSION_SECRET (or [console] session_secret in nanoguard.toml) to persist sessions across restarts."
        );
        config.console.session_secret = secret;
    }

    let db_path = &config.budget.db_path;
    let db = db::ConsoleDb::open(db_path)?;

    // Ensure the client_tokens table exists too (we may write tokens).
    db.with_conn(|conn| {
        token_store::migrate(conn)?;
        Ok(())
    })?;

    // Bootstrap admin if configured and no users exist.
    if let (Some(ref bootstrap), Some(pw)) = (
        &config.console.auth.local.bootstrap_admin,
        bootstrap_password,
    ) {
        if pw.len() < 12 {
            anyhow::bail!(
                "Bootstrap password for '{}' must be at least 12 characters",
                bootstrap.username
            );
        }
        if auth::is_common_password(&pw) {
            anyhow::bail!(
                "Bootstrap password for '{}' is too common",
                bootstrap.username
            );
        }
        let hash = auth::hash_password(&pw)?;
        let created =
            db.with_conn(|conn| db::maybe_bootstrap_admin(conn, &bootstrap.username, &hash))?;
        if created {
            tracing::info!(
                "Bootstrap admin '{}' provisioned. Clear ${} from the environment.",
                bootstrap.username,
                bootstrap.password_env
            );
        }
        // `pw` is a Zeroizing<String>; its buffer is overwritten with zeros
        // when it drops here. The plaintext is no longer in memory or in
        // the process environment image.
    }

    // Determine if cookies should be marked Secure.
    // If listening on loopback, no. Otherwise, yes.
    let listen = config.console.listen.parse::<SocketAddr>()?;
    let secure_cookie = !listen.ip().is_loopback();

    let audit_log = match audit::ConsoleAuditLog::open(&config.console.audit_path) {
        Ok(log) => {
            tracing::info!("console audit log: {}", config.console.audit_path);
            Some(log)
        }
        Err(e) => {
            tracing::warn!(
                "console audit log: failed to open {}: {}",
                config.console.audit_path,
                e
            );
            None
        }
    };

    let state = Arc::new(ConsoleState {
        config: config.clone(),
        db,
        secure_cookie,
        audit_log,
    });

    // Spawn a background task to prune expired and idle sessions every 5 minutes.
    let prune_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
        loop {
            interval.tick().await;
            let idle_hours = prune_state
                .config
                .console
                .session_idle_timeout_hours
                .unwrap_or(prune_state.config.console.session_ttl_hours);
            match prune_state.db.with_conn(db::prune_expired_sessions) {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("Pruned {} expired sessions", n);
                    }
                }
                Err(e) => tracing::warn!("Session prune failed: {}", e),
            }
            match prune_state
                .db
                .with_conn(|conn| db::prune_idle_sessions(conn, idle_hours))
            {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!("Pruned {} idle sessions", n);
                    }
                }
                Err(e) => tracing::warn!("Idle session prune failed: {}", e),
            }
            // Also prune stale login-attempt rows (older than 2x lockout window).
            let lockout_window = prune_state.config.console.lockout_duration_minutes;
            match prune_state
                .db
                .with_conn(|conn| db::prune_old_login_attempts(conn, lockout_window * 2))
            {
                Ok(n) => {
                    if n > 0 {
                        tracing::debug!("Pruned {} old login attempts", n);
                    }
                }
                Err(e) => tracing::warn!("Login attempt prune failed: {}", e),
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
        .route(
            "/api/tokens",
            get(handlers::api_list_tokens).post(handlers::api_create_token),
        )
        .route("/api/tokens/:id", delete(handlers::api_revoke_token))
        .route("/api/audit", get(handlers::api_audit))
        .route("/api/budget", get(handlers::api_budget))
        .route("/api/config", get(handlers::api_config))
        .route("/api/overview", get(handlers::api_overview))
        .route("/api/budget/limit", post(handlers::api_set_budget_limit))
        .route("/api/budget/reset", post(handlers::api_reset_budget_usage))
        .route(
            "/api/users",
            get(handlers::api_list_users).post(handlers::api_create_user),
        )
        .route("/api/users/:id", put(handlers::api_update_user))
        .route(
            "/api/users/:id/force-revoke-tokens",
            post(handlers::api_force_revoke_user_tokens),
        )
        .route("/api/edit", post(handlers::api_edit_file))
        .route("/api/validate", post(handlers::api_validate_file))
        .route("/api/backups", get(handlers::api_list_backups))
        .route("/api/revert", post(handlers::api_revert_file))
        .route("/api/reload/trigger", post(handlers::api_trigger_reload))
        .route("/api/reload/status", get(handlers::api_reload_status))
        .route("/api/console-audit", get(handlers::api_console_audit))
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
