use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;

use crate::{audit, backend, budget, client_auth, config, AppState};

/// Handles that survive a hot reload.
///
/// The proxy holds a budget DB connection, an audit log file handle, a
/// reqwest HTTP client, and a backend descriptor. None of these are
/// config-derived in a way that can be rebuilt safely while requests are
/// in flight (SQLite connection pooling, open file handles, HTTP
/// keep-alive pools). Reload reuses them as-is and only rebuilds the
/// matcher / redactor / guards / policy index on top.
///
/// Client-auth lives here too because its SQLite connection and its
/// in-memory verification cache must persist across reloads —
/// operators editing `[auth]` keys get a "restart-only" warning, same
/// pattern as `[backend]`.
#[derive(Clone)]
pub struct RuntimeHandles {
    /// Resolved backend pool. `build_app_state` rebuilds this from
    /// scratch on every reload; in-flight requests hold their own
    /// `Arc<AppState>` snapshot via `shared.load_full()`, so the prior
    /// pool (and its per-backend `reqwest::Client` connection pools)
    /// stays alive until the last in-flight request drops its Arc —
    /// no orphaning, no truncated responses. Both `[backends.*]` and
    /// `[routing]` are hot-reloadable on this path.
    pub pool: backend::BackendPoolRuntime,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
    pub audit: Option<Arc<audit::AuditLog>>,
    pub client_auth: Option<client_auth::ClientAuth>,
}

/// Shared, swappable handle to the live `AppState`.
///
/// Every request handler acquires its working snapshot once at the top of
/// the handler via `state.load_full()` and uses that `Arc<AppState>` for
/// the entire request lifetime, including streaming response paths. A
/// reload that happens mid-request does not affect the in-flight request
/// because the handler still holds the old `Arc`.
pub type SharedState = Arc<ArcSwap<AppState>>;

/// Build a fresh `AppState` from a config, reusing the given runtime
/// handles.
///
/// Used by both initial startup and the reload task. The runtime handles
/// (budget DB, audit log file, HTTP client, backend) are NOT rebuilt — a
/// reload that changes the relevant `[budget]` / `[audit]` / `[backend]`
/// keys ignores those keys with a warn log. The caller (startup or
/// reload) is responsible for emitting the warn.
pub fn build_app_state(mut cfg: config::Config, runtime: RuntimeHandles) -> Result<AppState> {
    use crate::{
        guard::{
            schema::{load_rules, RuleSpec, SchemaValidator, ViolationAction},
            spotlight::{default_rider, SpotlightConfig, SpotlightMethod},
            tool_gate::ToolGate,
        },
        matcher, policy, proxy,
    };
    use std::collections::{HashMap, HashSet};

    let policy_index =
        if let Some(path) = cfg.policies.bundle_path.as_ref().filter(|p| !p.is_empty()) {
            let policy = policy::Policy::from_path(path)?;
            tracing::info!(
                "policy bundle: loaded {} rule(s) from {} (name={:?})",
                policy.rule_count(),
                path,
                policy.metadata.name
            );
            policy::merge_into_keyword_config(&policy, &mut cfg.input.keyword);
            Some(Arc::new(policy::PolicyRuleIndex::from_policy(&policy)))
        } else {
            tracing::info!("policy bundle: not configured");
            None
        };

    let matchers = Arc::new(matcher::Matchers::build(&cfg.input.keyword)?);
    tracing::info!("matcher engine: {}", matchers.engine_name());

    let style = {
        let configured =
            proxy::redact::PlaceholderStyle::parse_name(&cfg.input.pii.placeholder_style);
        if cfg.input.pii.reversible && configured == proxy::redact::PlaceholderStyle::Bare {
            tracing::info!("redactor: reversible=true forces placeholder_style=indexed (was bare)");
            proxy::redact::PlaceholderStyle::Indexed
        } else {
            configured
        }
    };
    let redactor = Arc::new(proxy::redact::Redactor::build_with_style(
        &proxy::redact::default_inline_patterns(),
        &cfg.input.pii.dict_paths,
        style,
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

    let spotlight = if cfg.input.spotlight.enabled {
        let method = SpotlightMethod::parse_name(&cfg.input.spotlight.method)
            .unwrap_or(SpotlightMethod::Datamarking);
        let datamark_char = cfg
            .input
            .spotlight
            .datamark_char
            .chars()
            .next()
            .unwrap_or('^');
        let rider = cfg
            .input
            .spotlight
            .system_rider
            .clone()
            .unwrap_or_else(|| default_rider(method, datamark_char));
        let cfg_built = SpotlightConfig {
            method,
            untrusted_roles: cfg.input.spotlight.untrusted_roles.clone(),
            delimiter_open: cfg.input.spotlight.delimiter_open.clone(),
            delimiter_close: cfg.input.spotlight.delimiter_close.clone(),
            datamark_char,
            system_rider: rider,
        };
        tracing::info!(
            "spotlight: enabled (method={:?}, untrusted_roles={:?})",
            cfg_built.method,
            cfg_built.untrusted_roles
        );
        Some(Arc::new(cfg_built))
    } else {
        tracing::info!("spotlight: disabled");
        None
    };

    let schema = if cfg.output.schema.enabled {
        let specs: Vec<RuleSpec> = cfg
            .output
            .schema
            .rules
            .iter()
            .map(|r| RuleSpec {
                endpoint: r.endpoint.clone(),
                model_pattern: r.model_pattern.clone(),
                schema_path: r.schema_path.clone(),
                name: r.name.clone(),
            })
            .collect();
        let rules = load_rules(&specs)?;
        let action = ViolationAction::parse_name(&cfg.output.schema.on_violation);
        tracing::info!(
            "output schema: enabled ({} rule(s), on_violation={:?})",
            rules.len(),
            action
        );
        Some(Arc::new(SchemaValidator::new(rules, action)))
    } else {
        tracing::info!("output schema: disabled");
        None
    };

    let tool_gate = if cfg.tools.enabled {
        let mut schemas = HashMap::new();
        for spec in &cfg.tools.schemas {
            let raw = std::fs::read_to_string(&spec.schema_path)?;
            let schema_json: serde_json::Value = serde_json::from_str(&raw)?;
            let validator = jsonschema::draft202012::new(&schema_json)?;
            schemas.insert(spec.tool_name.clone(), validator);
        }
        let reject: HashSet<String> = cfg.tools.reject_entities.iter().cloned().collect();
        let mask: HashSet<String> = cfg.tools.mask_entities.iter().cloned().collect();
        let gate = ToolGate::new(
            cfg.tools.allow.clone(),
            cfg.tools.deny.clone(),
            schemas,
            redactor.clone(),
            reject,
            mask,
        );
        tracing::info!(
            "tool gate: enabled (allow={:?}, deny={} pattern(s), schemas={}, reject_entities={}, mask_entities={})",
            cfg.tools.allow,
            cfg.tools.deny.len(),
            cfg.tools.schemas.len(),
            cfg.tools.reject_entities.len(),
            cfg.tools.mask_entities.len(),
        );
        Some(Arc::new(gate))
    } else {
        tracing::info!("tool gate: disabled");
        None
    };

    // Hot reload semantics for the backend pool:
    //   - [backends.*] map AND [routing] are both rebuilt from the
    //     freshly-parsed config on every reload. In-flight requests
    //     hold an `Arc<AppState>` snapshot via `shared.load_full()`,
    //     so the prior `BackendPoolRuntime` (and the per-backend
    //     `reqwest::Client` connection pools it owns) stays alive
    //     until the last in-flight request drops its Arc. Dropping
    //     the old pool only releases the connection pools *after*
    //     they finish — no orphaning.
    //   - `Config::pool()` already validates: every [routing] rule
    //     references a known backend, [routing].default is one of
    //     them. A reload that would route to nothing fails here
    //     (the caller emits a reload_failed audit entry) rather
    //     than silently 400'ing every matching request post-swap.
    let (new_view, _warnings) = cfg.pool()?;
    let pool = crate::backend::BackendPoolRuntime::build(&new_view);

    Ok(AppState {
        config: cfg,
        matchers,
        redactor,
        pii_actions,
        spotlight,
        schema,
        tool_gate,
        policy: policy_index,
        pool,
        http_client: runtime.http_client,
        budget: runtime.budget,
        audit: runtime.audit,
        client_auth: runtime.client_auth,
    })
}

/// Run a single reload attempt on the blocking pool and emit an audit entry.
#[cfg(unix)]
async fn execute_reload(shared: &SharedState, runtime: &RuntimeHandles) -> Result<(), String> {
    let t0 = std::time::Instant::now();
    let shared_for_task = shared.clone();
    let runtime_for_task = runtime.clone();
    let result =
        tokio::task::spawn_blocking(move || reload_once(&shared_for_task, &runtime_for_task)).await;
    match result {
        Ok(Ok(())) => {
            let elapsed_us = t0.elapsed().as_micros() as u64;
            tracing::info!("hot reload: success ({}µs)", elapsed_us);
            emit_reload_audit(shared, true, None, elapsed_us);
            Ok(())
        }
        Ok(Err(e)) => {
            let elapsed_us = t0.elapsed().as_micros() as u64;
            let reason = classify_reload_error(&e);
            // Full chain is local-debug only; it can carry config /
            // policy / schema content that we do not want to persist.
            tracing::debug!("hot reload: error chain: {e:#}");
            tracing::warn!(
                "hot reload: FAILED ({}µs) — live state retained: {}",
                elapsed_us,
                reason
            );
            emit_reload_audit(shared, false, Some(reason.to_string()), elapsed_us);
            Err(reason.to_string())
        }
        Err(join_err) => {
            let elapsed_us = t0.elapsed().as_micros() as u64;
            tracing::warn!(
                "hot reload: blocking task failed ({}µs): {}",
                elapsed_us,
                join_err
            );
            emit_reload_audit(shared, false, Some("build_failed".to_string()), elapsed_us);
            Err("build_failed".to_string())
        }
    }
}

/// Run the SIGHUP-driven reload loop on the current task.
///
/// On each SIGHUP, re-read the config from the env-or-default path, run
/// `build_app_state`, and on success `ArcSwap::store` the new state. On
/// failure the live state is untouched and an `audit::Verdict::ReloadFailed`
/// entry is written (when audit is enabled).
///
/// SIGHUPs that arrive while a reload is in progress are coalesced into a
/// single follow-up reload via tokio's signal semantics (the underlying
/// stream collapses pending notifications).
#[cfg(unix)]
pub async fn run_reload_task(shared: SharedState, runtime: RuntimeHandles) {
    use tokio::signal::unix::{signal, SignalKind};

    let mut sig = match signal(SignalKind::hangup()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to install SIGHUP handler: {e}");
            return;
        }
    };

    tracing::info!("hot reload: SIGHUP listener installed");

    while sig.recv().await.is_some() {
        tracing::info!("hot reload: SIGHUP received, rebuilding state");
        let _ = execute_reload(&shared, &runtime).await;
    }
}

/// Run a Unix-domain-socket listener that accepts `RELOAD\n` and replies
/// `OK\n` or `ERR <reason>\n`.
///
/// The socket path is unlinked before bind so a stale socket from a prior
/// process does not block startup.
#[cfg(unix)]
pub async fn run_socket_reload_task(
    shared: SharedState,
    runtime: RuntimeHandles,
    socket_path: String,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    let path = std::path::Path::new(&socket_path);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(path) {
            tracing::warn!("failed to remove old reload socket file {socket_path}: {e}");
            return;
        }
    }
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("failed to create reload socket parent dir {parent:?}: {e}");
            return;
        }
    }

    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("failed to bind reload socket {socket_path}: {e}");
            return;
        }
    };

    tracing::info!("hot reload: Unix socket listener on {socket_path}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("reload socket accept error: {e}");
                continue;
            }
        };

        let shared = shared.clone();
        let runtime = runtime.clone();
        tokio::spawn(async move {
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut line = String::new();

            if let Err(e) = reader.read_line(&mut line).await {
                let _ = write_half
                    .write_all(format!("ERR read error: {e}\n").as_bytes())
                    .await;
                return;
            }

            match line.trim() {
                "RELOAD" => {
                    tracing::info!("hot reload: socket received RELOAD, rebuilding state");
                    match execute_reload(&shared, &runtime).await {
                        Ok(()) => {
                            let _ = write_half.write_all(b"OK\n").await;
                        }
                        Err(reason) => {
                            let _ = write_half
                                .write_all(format!("ERR {reason}\n").as_bytes())
                                .await;
                        }
                    }
                }
                "INVALIDATE_TOKENS" => {
                    // Flush the client-auth verification cache only.
                    // This is the lightweight cross-process notification
                    // the Web Console sends after revoking a token via
                    // `POST /api/tokens/:id DELETE`. RELOAD would also
                    // achieve invalidation as a side-effect of rebuilding
                    // AppState, but it pays for matcher / redactor /
                    // policy / schema reconstruction we don't need here.
                    let state = shared.load_full();
                    if let Some(auth) = state.client_auth.as_ref() {
                        auth.invalidate_all_cached();
                        tracing::info!(
                            "hot reload: socket received INVALIDATE_TOKENS, flushed client-auth cache"
                        );
                        let _ = write_half.write_all(b"OK\n").await;
                    } else {
                        // [auth] is disabled, so there is no cache to
                        // flush. Treat as a no-op success so the caller
                        // does not have to special-case the config.
                        tracing::info!(
                            "hot reload: socket received INVALIDATE_TOKENS, [auth] disabled — no-op"
                        );
                        let _ = write_half.write_all(b"OK\n").await;
                    }
                }
                _ => {
                    let _ = write_half
                        .write_all(b"ERR expected RELOAD or INVALIDATE_TOKENS\n")
                        .await;
                }
            }
        });
    }
}

/// Perform a single reload attempt.
///
/// On success the new state is atomically swapped into `shared` and the
/// previous state is dropped after every in-flight request that already
/// loaded it releases its `Arc`. On failure the live state is untouched
/// (all-or-nothing).
///
/// Restart-only keys (`[nanoguard].listen`, `[nanoguard].log_level`,
/// `[backend].*`, `[budget].*`, `[audit].*`) are detected by comparing
/// the new config against the live one. Changes are surfaced via
/// `tracing::warn` so an operator who pushes a TOML edit and SIGHUPs
/// doesn't get a silent `reload_ok` while their edit was actually
/// ignored on those fields.
#[cfg(unix)]
fn reload_once(shared: &SharedState, runtime: &RuntimeHandles) -> anyhow::Result<()> {
    let new_cfg = crate::config::Config::from_env_or_default()?;
    let live = shared.load_full();
    warn_on_restart_only_drift(&live.config, &new_cfg);
    let candidate = build_app_state(new_cfg, runtime.clone())?;
    shared.store(Arc::new(candidate));
    Ok(())
}

/// Compare restart-only keys between the live and incoming config; emit
/// a single `tracing::warn` listing any that drift.
///
/// The build_app_state docstring promises this; reload_once is the
/// caller that has to deliver it. The list of restart-only keys here
/// must stay in sync with `docs/design/hot-reload.md > What is not
/// reloadable, and why` and with the `docs/operations.md` runbook.
/// Diff the live and incoming configs for keys that the reload path
/// does not honor in-place, and emit a single combined warning so the
/// operator sees which edits were ignored.
#[cfg(unix)]
fn warn_on_restart_only_drift(live: &crate::config::Config, new: &crate::config::Config) {
    let mut ignored: Vec<&'static str> = Vec::new();

    if live.nanoguard.listen != new.nanoguard.listen {
        ignored.push("[nanoguard].listen");
    }
    if live.nanoguard.log_level != new.nanoguard.log_level {
        ignored.push("[nanoguard].log_level");
    }
    // Both the legacy single `[backend]` and the new `[backends.*]`
    // are hot-reloadable as of the pool-rebuild path in
    // `build_app_state`. `cfg.pool()` synthesizes a
    // `[backends.default]` from any legacy `[backend]` and rebuilds
    // the live `BackendPoolRuntime`. Old `reqwest::Client` pools
    // stay alive on the prior `AppState` snapshot until the last
    // in-flight request drops its Arc, so no warning fires for
    // backend-pool drift.
    if live.budget.enabled != new.budget.enabled {
        ignored.push("[budget].enabled");
    }
    if live.budget.db_path != new.budget.db_path {
        ignored.push("[budget].db_path");
    }
    if live.audit.enabled != new.audit.enabled {
        ignored.push("[audit].enabled");
    }
    if live.audit.path != new.audit.path {
        ignored.push("[audit].path");
    }
    if live.audit.hash_only != new.audit.hash_only {
        ignored.push("[audit].hash_only");
    }
    // [auth].* is restart-only in v1: the SQLite handle for client_tokens
    // and the verification cache are runtime state owned by
    // RuntimeHandles. Live toggling of `enabled` is plausible but easy
    // to misuse (flip on before any tokens exist → lock everyone out),
    // so we keep all of [auth] behind a restart for the first iteration.
    if live.auth.enabled != new.auth.enabled {
        ignored.push("[auth].enabled");
    }
    if live.auth.env_marker != new.auth.env_marker {
        ignored.push("[auth].env_marker");
    }
    if live.auth.cache_capacity != new.auth.cache_capacity {
        ignored.push("[auth].cache_capacity");
    }
    if live.auth.cache_ttl_secs != new.auth.cache_ttl_secs {
        ignored.push("[auth].cache_ttl_secs");
    }
    if live.auth.require_https != new.auth.require_https {
        ignored.push("[auth].require_https");
    }

    if !ignored.is_empty() {
        tracing::warn!(
            "hot reload: restart-only key(s) changed in config — \
             change(s) IGNORED until process restart: {}",
            ignored.join(", ")
        );
    }
}

#[cfg(unix)]
fn emit_reload_audit(shared: &SharedState, ok: bool, error: Option<String>, latency_us: u64) {
    let state = shared.load_full();
    let Some(audit) = state.audit.as_ref() else {
        return;
    };
    audit.write_reload(ok, error, latency_us);
}

/// Classify an anyhow reload error into a short, bounded label.
///
/// The full `Display` of an anyhow error chain can include the offending
/// TOML / YAML line text, regex pattern, JSON Schema body, or filesystem
/// path. None of that should land in the audit log, which is meant to be
/// shippable to compliance pipelines. The classifier returns one of a
/// small enum-like set of strings; the full error chain stays in
/// `tracing::debug` (volatile, local).
#[cfg(unix)]
fn classify_reload_error(e: &anyhow::Error) -> &'static str {
    for cause in e.chain() {
        if cause.is::<toml::de::Error>() {
            return "config_parse_error";
        }
        if cause.is::<serde_yaml::Error>() {
            return "policy_yaml_parse_error";
        }
        if cause.is::<regex::Error>() {
            return "regex_compile_error";
        }
        if cause.is::<serde_json::Error>() {
            return "json_parse_error";
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return match io.kind() {
                std::io::ErrorKind::NotFound => "file_not_found",
                std::io::ErrorKind::PermissionDenied => "file_permission_denied",
                _ => "io_error",
            };
        }
    }
    "build_failed"
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    #[cfg(unix)]
    async fn socket_reload_responds_ok_or_err() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        let socket_path = format!("/tmp/ng-sock-test-{:x}", rand::random::<u32>());
        let _ = std::fs::remove_file(&socket_path);

        let cfg = crate::config::Config {
            nanoguard: crate::config::ServerConfig::default(),
            backend: Some(crate::config::BackendConfig {
                provider: "ollama".to_string(),
                endpoint: "http://localhost:11434".to_string(),
                api_key: None,
                model: None,
            }),
            backends: std::collections::BTreeMap::new(),
            routing: crate::config::RoutingConfig::default(),
            input: crate::config::InputConfig::default(),
            output: crate::config::OutputConfig::default(),
            budget: crate::config::BudgetConfig::default(),
            audit: crate::config::AuditConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            policies: crate::config::PoliciesConfig::default(),
            auth: crate::config::AuthConfig::default(),
            console: crate::config::ConsoleConfig {
                enabled: true,
                listen: crate::config::ConsoleConfig::default_listen(),
                session_secret: "test-secret-for-unit-tests-only-do-not-use".to_string(),
                session_ttl_hours: 24,
                audit_path: "console-audit.jsonl".to_string(),
                backup_limit: 20,
                session_idle_timeout_hours: None,
                max_login_attempts: 10,
                lockout_duration_minutes: 15,
                auth: crate::config::ConsoleAuthConfig::default(),
            },
            reload: crate::config::ReloadConfig::default(),
        };
        let (pool_view, _) = cfg.pool().expect("test config produces a backend pool");
        let pool = crate::backend::BackendPoolRuntime::build(&pool_view);
        let http_client = reqwest::Client::new();
        let runtime = RuntimeHandles {
            pool,
            http_client,
            budget: None,
            audit: None,
            client_auth: None,
        };

        let state = build_app_state(cfg, runtime.clone()).unwrap();
        let shared: SharedState = Arc::new(ArcSwap::from_pointee(state));

        let listener_task =
            tokio::spawn(run_socket_reload_task(shared, runtime, socket_path.clone()));

        // Give the listener time to bind.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(b"RELOAD\n").await.unwrap();
        stream.flush().await.unwrap();

        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(
            resp.starts_with("OK") || resp.starts_with("ERR"),
            "unexpected response: {resp}"
        );

        listener_task.abort();
        let _ = std::fs::remove_file(&socket_path);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn socket_reload_rejects_invalid_command() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::UnixStream;

        let socket_path = format!("/tmp/ng-sock-badcmd-{:x}", rand::random::<u32>());
        let _ = std::fs::remove_file(&socket_path);

        let cfg = crate::config::Config {
            nanoguard: crate::config::ServerConfig::default(),
            backend: Some(crate::config::BackendConfig {
                provider: "ollama".to_string(),
                endpoint: "http://localhost:11434".to_string(),
                api_key: None,
                model: None,
            }),
            backends: std::collections::BTreeMap::new(),
            routing: crate::config::RoutingConfig::default(),
            input: crate::config::InputConfig::default(),
            output: crate::config::OutputConfig::default(),
            budget: crate::config::BudgetConfig::default(),
            audit: crate::config::AuditConfig::default(),
            tools: crate::config::ToolsConfig::default(),
            policies: crate::config::PoliciesConfig::default(),
            auth: crate::config::AuthConfig::default(),
            console: crate::config::ConsoleConfig {
                enabled: true,
                listen: crate::config::ConsoleConfig::default_listen(),
                session_secret: "test-secret-for-unit-tests-only-do-not-use".to_string(),
                session_ttl_hours: 24,
                audit_path: "console-audit.jsonl".to_string(),
                backup_limit: 20,
                session_idle_timeout_hours: None,
                max_login_attempts: 10,
                lockout_duration_minutes: 15,
                auth: crate::config::ConsoleAuthConfig::default(),
            },
            reload: crate::config::ReloadConfig::default(),
        };
        let (pool_view, _) = cfg.pool().expect("test config produces a backend pool");
        let pool = crate::backend::BackendPoolRuntime::build(&pool_view);
        let http_client = reqwest::Client::new();
        let runtime = RuntimeHandles {
            pool,
            http_client,
            budget: None,
            audit: None,
            client_auth: None,
        };

        let state = build_app_state(cfg, runtime.clone()).unwrap();
        let shared: SharedState = Arc::new(ArcSwap::from_pointee(state));

        let listener_task =
            tokio::spawn(run_socket_reload_task(shared, runtime, socket_path.clone()));

        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut stream = UnixStream::connect(&socket_path).await.unwrap();
        stream.write_all(b"PING\n").await.unwrap();
        stream.flush().await.unwrap();

        let mut buf = [0u8; 256];
        let n = stream.read(&mut buf).await.unwrap();
        let resp = String::from_utf8_lossy(&buf[..n]);
        assert!(
            resp.starts_with("ERR expected RELOAD"),
            "unexpected response: {resp}"
        );

        listener_task.abort();
        let _ = std::fs::remove_file(&socket_path);
    }
}
