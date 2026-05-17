use anyhow::Result;
use arc_swap::ArcSwap;
use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use nanoguard::{
    admin, audit, backend, budget, build_app_state, config, console, proxy, reload, RuntimeHandles,
    SharedState,
};

fn main() -> Result<()> {
    // Pre-runtime: load the config, then run the console's pre-tokio
    // prep so the BOOTSTRAP_PASSWORD plaintext is wrapped in a
    // Zeroizing<String> (deterministic wipe after hashing) and
    // CONSOLE_SESSION_SECRET length is validated before we bind. The
    // env-slot itself stays visible in /proc/<pid>/environ until the
    // operator `unset`s the variable; the Zeroizing wrap only kills
    // the in-memory copy. README's bootstrap section asks the
    // operator to clear it after the first run.
    // If the operator turned the console off (`[console].enabled =
    // false`) the prep is still safe — it only reads env and never
    // requires a configured bootstrap admin.
    let mut cfg = config::Config::from_env_or_default()?;
    let console_bootstrap_password = console::prepare_for_run(&mut cfg)?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async_main(cfg, console_bootstrap_password))
}

async fn async_main(
    cfg: config::Config,
    console_bootstrap_password: Option<zeroize::Zeroizing<String>>,
) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("RUST_LOG")
                .unwrap_or_else(|_| EnvFilter::new(&cfg.nanoguard.log_level)),
        )
        .init();

    // Resolve the multi-backend pool from TOML. `Config::pool()`
    // handles legacy `[backend]` → synthesized `[backends.default]`
    // migration, validates routing rule references, and emits
    // operator-readable warnings (both schemas present, missing
    // default, etc.) that we surface here instead of swallowing.
    let (backend_pool_view, pool_warnings) = cfg.pool()?;
    for w in &pool_warnings {
        tracing::warn!("{w}");
    }
    let pool = backend::BackendPoolRuntime::build(&backend_pool_view);
    let n_backends = pool.backends.len();
    let default_label = pool.default_backend.clone();
    tracing::info!(
        "backends: {} configured (default = {})",
        n_backends,
        default_label
    );
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
        pool,
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
    let default_backend_provider = backend_pool_view
        .backends
        .get(&default_label)
        .map(|b| b.provider.clone())
        .unwrap_or_default();
    let default_backend_endpoint = backend_pool_view
        .backends
        .get(&default_label)
        .map(|b| b.endpoint.clone())
        .unwrap_or_default();

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
    tracing::info!("default backend: {default_backend_provider} → {default_backend_endpoint}");

    // Write PID file if configured, so the console can send SIGHUP.
    if let Some(ref pid_file) = cfg.reload.pid_file {
        if let Some(parent) = std::path::Path::new(pid_file).parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                tracing::warn!(
                    "failed to create pid file parent directory {:?}: {}",
                    parent,
                    e
                );
            }
        }
        if let Err(e) = std::fs::write(pid_file, format!("{}\n", std::process::id())) {
            tracing::warn!("failed to write pid file {}: {}", pid_file, e);
        } else {
            tracing::info!("pid file written to {}", pid_file);
        }
    }

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

    // Optional Unix-domain-socket reload listener.
    #[cfg(unix)]
    let _socket_task = if let Some(ref socket_path) = cfg.reload.socket {
        let shared = shared.clone();
        let runtime = runtime.clone();
        let socket_path = socket_path.clone();
        Some(tokio::spawn(async move {
            reload::run_socket_reload_task(shared, runtime, socket_path).await;
        }))
    } else {
        None
    };

    // Single-process boot: when [console].enabled (default true)
    // we spawn the Web Configuration UI on the same tokio runtime
    // so `make run` alone gives an operator both ports. The
    // `nanoguard-console` binary stays as the "console only"
    // deployment (operator workstation pointing at a remote DB).
    //
    // The console task is fire-and-forget: when the proxy's
    // graceful shutdown completes, the runtime drops with the
    // console task still bound — no extra coordination needed for
    // Ctrl+C / SIGTERM because both surfaces install the same
    // signal handler via `shutdown_signal()`.
    let console_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = if cfg.console.enabled {
        let console_cfg = cfg.clone();
        let pw = console_bootstrap_password;
        Some(tokio::spawn(
            async move { console::run(console_cfg, pw).await },
        ))
    } else {
        tracing::info!("[console].enabled = false; not spawning the console listener");
        // Drop the password explicitly so its Zeroizing<String>
        // wipes the buffer now, not at the end of main().
        drop(console_bootstrap_password);
        None
    };

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let proxy_serve = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    // Race the proxy server against the console task. If the console
    // exits early — bind error, panic, anything — and the operator
    // asked for it (`[console].enabled = true`), kill the proxy too:
    // a partial boot (proxy up, console silently dead) is exactly
    // the state we want to fail loud on, not silently tolerate.
    // When console is disabled the JoinHandle is None and we just
    // await the proxy normally.
    if let Some(handle) = console_task {
        tokio::select! {
            proxy_result = proxy_serve => {
                proxy_result?;
            }
            console_result = handle => {
                match console_result {
                    Ok(Ok(())) => {
                        tracing::warn!(
                            "console listener exited cleanly while proxy was running; \
                             treating as fatal because [console].enabled = true"
                        );
                        anyhow::bail!("console listener exited unexpectedly");
                    }
                    Ok(Err(e)) => {
                        tracing::error!("console listener failed: {e:#}");
                        anyhow::bail!("console listener failed: {e:#}");
                    }
                    Err(join_err) => {
                        tracing::error!("console listener task panicked: {join_err}");
                        anyhow::bail!("console listener task panicked");
                    }
                }
            }
        }
    } else {
        proxy_serve.await?;
    }

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
