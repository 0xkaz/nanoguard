use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use nanoguard::{admin, audit, backend, budget, config, matcher, proxy, AppState};

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
    tracing::info!("matcher engine: {}", matchers.engine_name());
    let redactor = Arc::new(proxy::redact::Redactor::build_with_style(
        &proxy::redact::default_inline_patterns(),
        &cfg.input.pii.dict_paths,
        proxy::redact::PlaceholderStyle::from_str(&cfg.input.pii.placeholder_style),
    )?);
    let pii_actions = Arc::new(proxy::redact::partition_by_action(
        &redactor.entity_names(),
        &cfg.input.pii.entities,
        &cfg.input.pii.action,
    ));
    tracing::info!(
        "redactor: {} entity rules, style={:?}, mask={}, reject={}, log={}",
        redactor.rule_count(),
        redactor.style(),
        pii_actions.mask.len(),
        pii_actions.reject.len(),
        pii_actions.log.len(),
    );
    let backend = backend::Backend::new(cfg.backend.clone());
    let http_client = reqwest::Client::builder().use_rustls_tls().build()?;

    let budget = if cfg.budget.enabled {
        tracing::info!("budget: enabled (db={})", cfg.budget.db_path);
        Some(budget::build(&cfg.budget).await?)
    } else {
        tracing::info!("budget: disabled");
        None
    };

    let audit_log = if cfg.audit.enabled {
        tracing::info!(
            "audit: enabled (path={}, hash_only={})",
            cfg.audit.path,
            cfg.audit.hash_only
        );
        Some(audit::AuditLog::open(&cfg.audit)?)
    } else {
        tracing::info!("audit: disabled");
        None
    };

    let state = Arc::new(AppState {
        config: cfg.clone(),
        matchers,
        redactor,
        pii_actions,
        backend,
        http_client,
        budget,
        audit: audit_log,
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
