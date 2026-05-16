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
    pub backend: backend::Backend,
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

    Ok(AppState {
        config: cfg,
        matchers,
        redactor,
        pii_actions,
        spotlight,
        schema,
        tool_gate,
        policy: policy_index,
        backend: runtime.backend,
        http_client: runtime.http_client,
        budget: runtime.budget,
        audit: runtime.audit,
        client_auth: runtime.client_auth,
    })
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
        let t0 = std::time::Instant::now();
        // The config + dict + policy reads and the matcher / redactor /
        // schema rebuilds are all blocking I/O and CPU work. Run them
        // on the blocking thread pool so they cannot stall the Tokio
        // worker that's also responsible for the SIGHUP stream and the
        // graceful-shutdown listener.
        let shared_for_task = shared.clone();
        let runtime_for_task = runtime.clone();
        let result =
            tokio::task::spawn_blocking(move || reload_once(&shared_for_task, &runtime_for_task))
                .await;
        match result {
            Ok(Ok(())) => {
                let elapsed_us = t0.elapsed().as_micros() as u64;
                tracing::info!("hot reload: success ({}µs)", elapsed_us);
                emit_reload_audit(&shared, true, None, elapsed_us);
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
                emit_reload_audit(&shared, false, Some(reason.to_string()), elapsed_us);
            }
            Err(join_err) => {
                let elapsed_us = t0.elapsed().as_micros() as u64;
                tracing::warn!(
                    "hot reload: blocking task failed ({}µs): {}",
                    elapsed_us,
                    join_err
                );
                emit_reload_audit(&shared, false, Some("build_failed".to_string()), elapsed_us);
            }
        }
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
#[cfg(unix)]
fn warn_on_restart_only_drift(live: &crate::config::Config, new: &crate::config::Config) {
    let mut ignored: Vec<&'static str> = Vec::new();

    if live.nanoguard.listen != new.nanoguard.listen {
        ignored.push("[nanoguard].listen");
    }
    if live.nanoguard.log_level != new.nanoguard.log_level {
        ignored.push("[nanoguard].log_level");
    }
    if live.backend.provider != new.backend.provider {
        ignored.push("[backend].provider");
    }
    if live.backend.endpoint != new.backend.endpoint {
        ignored.push("[backend].endpoint");
    }
    if live.backend.api_key != new.backend.api_key {
        ignored.push("[backend].api_key");
    }
    if live.backend.model != new.backend.model {
        ignored.push("[backend].model");
    }
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
