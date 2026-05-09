use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use nanoguard::{admin, backend, budget, config, matcher, proxy, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::Config::from_env_or_default()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new(&cfg.nanoguard.log_level)),
        )
        .init();

    let matchers = Arc::new(matcher::Matchers::build(&cfg.input.keyword)?);
    let backend = backend::Backend::new(cfg.backend.clone());
    let http_client = reqwest::Client::builder().use_rustls_tls().build()?;

    let budget = if cfg.budget.enabled {
        tracing::info!("budget: enabled (db={})", cfg.budget.db_path);
        Some(budget::build(&cfg.budget).await?)
    } else {
        tracing::info!("budget: disabled");
        None
    };

    let state = Arc::new(AppState {
        config: cfg.clone(),
        matchers,
        backend,
        http_client,
        budget,
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/messages", post(proxy::anthropic::messages))
        .route("/v1/models", get(proxy::list_models))
        .route("/health", get(|| async { "ok" }))
        .route("/v1/admin/budget/:api_key", get(admin::get_budget))
        .route(
            "/v1/admin/budget/:api_key",
            axum::routing::put(admin::set_budget),
        )
        .route(
            "/v1/admin/budget/:api_key/reset",
            axum::routing::delete(admin::reset_budget),
        )
        .with_state(state);

    let addr = cfg.nanoguard.listen.parse::<std::net::SocketAddr>()?;
    tracing::info!("nanoguard listening on http://{addr}");
    tracing::info!(
        "backend: {} → {}",
        cfg.backend.provider,
        cfg.backend.endpoint
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!("nanoguard stopped gracefully");
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
