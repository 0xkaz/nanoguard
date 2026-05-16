use anyhow::Result;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

fn main() -> Result<()> {
    // Load config synchronously before starting the async runtime.
    let cfg = nanoguard::config::Config::from_env_or_default()?;

    // Read the bootstrap password BEFORE starting tokio so the plaintext
    // never lingers in the process environment image (/proc/<pid>/environ).
    // It is wrapped in Zeroizing<String> so the buffer is overwritten with
    // zeros when it is dropped after hashing.
    let bootstrap_password = if let Some(ref bootstrap) = cfg.console.auth.local.bootstrap_admin {
        std::env::var(&bootstrap.password_env)
            .ok()
            .map(Zeroizing::new)
    } else {
        None
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        tracing_subscriber::fmt()
            .with_env_filter(
                EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
            )
            .init();

        nanoguard::console::run(cfg, bootstrap_password).await
    })
}
