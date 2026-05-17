use anyhow::Result;
use tracing_subscriber::EnvFilter;
use zeroize::Zeroizing;

fn main() -> Result<()> {
    // Load config synchronously before starting the async runtime.
    let mut cfg = nanoguard::config::Config::from_env_or_default()?;

    // CONSOLE_SESSION_SECRET in the environment overrides whatever the TOML
    // file carries. This is the 12-factor pattern: secrets travel via env so
    // they don't have to be committed to the config file, and TOML stays
    // dev-safe (the bundled `nanoguard.toml` sets session_secret = "" which
    // triggers ephemeral-secret generation downstream in `console::run`).
    if let Ok(env_secret) = std::env::var("CONSOLE_SESSION_SECRET") {
        if !env_secret.is_empty() {
            cfg.console.session_secret = env_secret;
        }
    }

    // The cookie::Key used for signing session cookies requires >= 32 bytes.
    // If the operator passed something shorter (via env or TOML), the
    // worker-thread panic happens silently per-request and the console looks
    // broken without an obvious cause. Fail early with a clear message.
    if !cfg.console.session_secret.is_empty() && cfg.console.session_secret.len() < 32 {
        anyhow::bail!(
            "[console] session_secret must be at least 32 bytes (got {}). \
             Generate one with: openssl rand -hex 32",
            cfg.console.session_secret.len()
        );
    }

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
