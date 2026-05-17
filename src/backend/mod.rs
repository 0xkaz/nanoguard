use crate::config::BackendConfig;
use anyhow::Result;
use reqwest::Client;
use serde_json::Value;

#[derive(Clone)]
pub struct Backend {
    client: Client,
    cfg: BackendConfig,
}

impl Backend {
    pub fn new(cfg: BackendConfig) -> Self {
        Self {
            client: Client::builder()
                .use_rustls_tls()
                .build()
                .expect("reqwest client"),
            cfg,
        }
    }

    /// The backend endpoint this `Backend` is configured to forward to.
    ///
    /// Reads from the `BackendConfig` snapshot frozen into the handle at
    /// startup. Hot reload preserves `Backend` across SIGHUP because
    /// `reqwest::Client` owns a connection pool that should not be
    /// orphaned mid-request; consequently this value never changes
    /// during the process lifetime. Use this for *anything* the proxy
    /// surfaces externally (e.g. `/v1/models`) so reads and writes
    /// always point at the same upstream.
    pub fn endpoint(&self) -> &str {
        self.cfg.endpoint.trim_end_matches('/')
    }

    pub async fn forward_chat(&self, mut body: Value) -> Result<reqwest::Response> {
        let url = format!(
            "{}/v1/chat/completions",
            self.cfg.endpoint.trim_end_matches('/')
        );

        // Inject model if caller didn't specify one
        if let Some(model) = &self.cfg.model {
            if body.get("model").is_none() {
                body["model"] = Value::String(model.clone());
            }
        }

        let mut req = self.client.post(&url).json(&body);
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }

        Ok(req.send().await?)
    }

    pub fn provider(&self) -> &str {
        &self.cfg.provider
    }

    /// Configured api_key for this backend, if any. Used by the
    /// /v1/models aggregator (and any future GET that talks to the
    /// upstream directly) so authenticated providers like OpenAI /
    /// Anthropic / DeepSeek answer the request instead of returning
    /// 401.
    pub fn api_key(&self) -> Option<&str> {
        self.cfg.api_key.as_deref()
    }
}

/// Runtime view of the resolved backend pool. Holds a `Backend`
/// (which owns its own `reqwest::Client` connection pool) per
/// configured label, plus the routing table the proxy consults on
/// every request to pick which backend a given `model` goes to.
///
/// This is the value the proxy hot path actually queries via
/// `state.pool.route(model)` and `state.pool.get(name)`. The TOML
/// `Config::pool()` builder constructs the matching `BackendPool`
/// view that this `BackendPoolRuntime` mirrors.
#[derive(Clone)]
pub struct BackendPoolRuntime {
    pub backends: std::collections::BTreeMap<String, Backend>,
    pub rules: Vec<crate::config::RoutingRule>,
    pub default_backend: String,
}

impl BackendPoolRuntime {
    /// Build the runtime pool from a `[backends.*]` map + routing
    /// declaration. Each backend gets its own `Backend` (=> its own
    /// `reqwest::Client` connection pool).
    pub fn build(pool: &crate::config::BackendPool) -> Self {
        let backends = pool
            .backends
            .iter()
            .map(|(name, cfg)| (name.clone(), Backend::new(cfg.clone())))
            .collect();
        Self {
            backends,
            rules: pool.rules.clone(),
            default_backend: pool.default_backend.clone(),
        }
    }

    /// Resolve a request's model field to a Backend handle. Returns
    /// `None` only if the routing default points at a name that was
    /// dropped from `[backends.*]` between parse and resolve — that
    /// should be impossible because `Config::pool()` validates.
    pub fn route(&self, model: Option<&str>) -> Option<&Backend> {
        let name = self.route_name(model)?;
        self.backends.get(name)
    }

    /// Resolve a request's model field to a backend label. Same
    /// rules as `Config::pool().route(...)`; duplicated here so the
    /// proxy hot path does not have to call back into config.
    pub fn route_name(&self, model: Option<&str>) -> Option<&str> {
        let Some(model) = model else {
            return Some(self.default_backend.as_str());
        };
        for r in &self.rules {
            if rule_matches(&r.model, model) {
                return Some(r.backend.as_str());
            }
        }
        Some(self.default_backend.as_str())
    }

    /// Get a backend by label.
    pub fn get(&self, name: &str) -> Option<&Backend> {
        self.backends.get(name)
    }

    /// Default backend label.
    pub fn default_backend(&self) -> &str {
        &self.default_backend
    }
}

fn rule_matches(pattern: &str, model: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        model.starts_with(prefix)
    } else {
        pattern == model
    }
}
