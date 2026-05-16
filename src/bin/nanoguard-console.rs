use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let cfg = nanoguard::config::Config::from_env_or_default()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    nanoguard::console::run(cfg).await
}
