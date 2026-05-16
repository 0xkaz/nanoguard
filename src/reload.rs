use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;

use crate::{audit, backend, budget, config, AppState};

/// Handles that survive a hot reload.
///
/// The proxy holds a budget DB connection, an audit log file handle, a
/// reqwest HTTP client, and a backend descriptor. None of these are
/// config-derived in a way that can be rebuilt safely while requests are
/// in flight (SQLite connection pooling, open file handles, HTTP
/// keep-alive pools). Reload reuses them as-is and only rebuilds the
/// matcher / redactor / guards / policy index on top.
#[derive(Clone)]
pub struct RuntimeHandles {
    pub backend: backend::Backend,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
    pub audit: Option<Arc<audit::AuditLog>>,
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
        match reload_once(&shared, &runtime) {
            Ok(()) => {
                let elapsed_us = t0.elapsed().as_micros() as u64;
                tracing::info!("hot reload: success ({}µs)", elapsed_us);
                emit_reload_audit(&shared, true, None, elapsed_us);
            }
            Err(e) => {
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
        }
    }
}

/// Perform a single reload attempt.
///
/// On success the new state is atomically swapped into `shared` and the
/// previous state is dropped after every in-flight request that already
/// loaded it releases its `Arc`. On failure the live state is untouched
/// (all-or-nothing).
#[cfg(unix)]
fn reload_once(shared: &SharedState, runtime: &RuntimeHandles) -> anyhow::Result<()> {
    let cfg = crate::config::Config::from_env_or_default()?;
    let candidate = build_app_state(cfg, runtime.clone())?;
    shared.store(Arc::new(candidate));
    Ok(())
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
