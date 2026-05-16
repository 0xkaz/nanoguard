use anyhow::Result;
use arc_swap::ArcSwap;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use nanoguard::{
    admin, audit, backend, budget, build_app_state, config, proxy, reload, RuntimeHandles,
    SharedState,
};

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::Config::from_env_or_default()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new(&cfg.nanoguard.log_level)),
        )
        .init();

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

    // Client-auth: opens the client_tokens table when [auth].enabled OR
    // [budget].enabled (the admin-token-issuance endpoints will live on
    // the budget DB and are useful even before enforcement is turned on,
    // so the table exists as soon as there is a DB to put it in). When
    // both are off, no table is opened — defaults stay zero-cost.
    let client_auth = if cfg.auth.enabled || cfg.budget.enabled {
        let db_path = &cfg.budget.db_path;
        tracing::info!(
            "client_auth: opening client_tokens on {} (enforcement={})",
            db_path,
            cfg.auth.enabled
        );
        Some(nanoguard::client_auth::ClientAuth::open(
            db_path,
            cfg.auth.clone(),
        )?)
    } else {
        tracing::info!("client_auth: disabled (no DB)");
        None
    };

    let runtime = RuntimeHandles {
        backend,
        http_client,
        budget,
        audit: audit_log,
        client_auth,
    };

    let initial_state = build_app_state(cfg.clone(), runtime.clone())?;
    let shared: SharedState = Arc::new(ArcSwap::from_pointee(initial_state));

    // Capture the listen address + log level from the initial config; these
    // are restart-only keys, so the values frozen here remain authoritative
    // for the process lifetime even if a later reload changes them on disk.
    let listen = cfg.nanoguard.listen.clone();
    let provider = cfg.backend.provider.clone();
    let endpoint = cfg.backend.endpoint.clone();

    // Routes that go through client-token verification (when
    // [auth].enabled = true; the middleware short-circuits otherwise).
    // /v1/models is gated too — listing models is privileged information
    // once token-scoped allowed_models lands.
    let protected = Router::new()
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/messages", post(proxy::anthropic::messages))
        .route("/v1/models", get(proxy::list_models))
        .layer(axum::middleware::from_fn_with_state(
            shared.clone(),
            nanoguard::client_auth::verify_request,
        ));

    // Public + admin-gated routes. /health stays unauthenticated for LB
    // probes. /v1/admin/* has its own ADMIN_API_KEY Bearer check inside
    // each handler — adding the client-token layer here would require
    // operators to mint a client token before they could even configure
    // budgets, which inverts the bootstrap order.
    let public_and_admin = Router::new()
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
        // Client token management (see docs/design/client-auth.md).
        // Gated by the same ADMIN_API_KEY check as the budget endpoints,
        // so bootstrap order is: configure [budget].admin_api_key first,
        // then POST /v1/admin/clients to mint operator tokens.
        .route(
            "/v1/admin/clients",
            post(admin::create_client).get(admin::list_clients),
        )
        .route(
            "/v1/admin/clients/:id",
            axum::routing::delete(admin::revoke_client),
        );

    let app = protected.merge(public_and_admin).with_state(shared.clone());

    let addr = listen.parse::<std::net::SocketAddr>()?;
    tracing::info!("nanoguard listening on http://{addr}");
    tracing::info!("backend: {provider} → {endpoint}");

    // SIGHUP-driven hot reload (Unix only). On Ctrl+C / SIGTERM we let the
    // axum graceful shutdown drain instead.
    #[cfg(unix)]
    let _reload_task = {
        let shared = shared.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            reload::run_reload_task(shared, runtime).await;
        })
    };

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
