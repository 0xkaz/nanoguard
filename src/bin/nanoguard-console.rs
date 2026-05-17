use anyhow::Result;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Load config synchronously before starting the async runtime,
    // and run the pre-tokio prep (CONSOLE_SESSION_SECRET env override,
    // session_secret length validation, BOOTSTRAP_PASSWORD read into
    // Zeroizing<String>). The same helper runs inside the unified
    // `nanoguard` binary too — keeping the two binaries on one code
    // path so they cannot drift apart in subtle (and security-
    // sensitive) ways.
    let mut cfg = nanoguard::config::Config::from_env_or_default()?;
    let bootstrap_password = nanoguard::console::prepare_for_run(&mut cfg)?;

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
