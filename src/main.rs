use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

mod backend;
mod config;
mod matcher;
mod proxy;

pub struct AppState {
    pub config: config::Config,
    pub matchers: matcher::Matchers,
    pub backend: backend::Backend,
    pub http_client: reqwest::Client,
}

impl AppState {
    pub fn backend_endpoint(&self) -> &str {
        self.config.backend.endpoint.trim_end_matches('/')
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = config::Config::from_env_or_default()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new(&cfg.nanoguard.log_level)),
        )
        .init();

    let matchers = matcher::Matchers::build(&cfg.input.keyword)?;
    let backend = backend::Backend::new(cfg.backend.clone());
    let http_client = reqwest::Client::builder().use_rustls_tls().build()?;

    let state = Arc::new(AppState {
        config: cfg.clone(),
        matchers,
        backend,
        http_client,
    });

    let app = Router::new()
        .route("/v1/chat/completions", post(proxy::chat_completions))
        .route("/v1/models", get(proxy::list_models))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let addr = cfg.nanoguard.listen.parse::<std::net::SocketAddr>()?;
    tracing::info!("nanoguard listening on http://{addr}");
    tracing::info!(
        "backend: {} → {}",
        cfg.backend.provider,
        cfg.backend.endpoint
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
