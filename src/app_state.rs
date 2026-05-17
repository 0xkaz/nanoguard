use std::sync::Arc;

use crate::{audit, backend, budget, client_auth, config, matcher, proxy};

pub struct AppState {
    pub config: config::Config,
    pub matchers: Arc<matcher::Matchers>,
    pub redactor: Arc<proxy::redact::Redactor>,
    /// Pre-computed entity buckets per per-entity action override.
    pub pii_actions: Arc<proxy::redact::ActionPartition>,
    /// Pre-computed spotlight config; None when spotlight is disabled.
    pub spotlight: Option<Arc<crate::guard::spotlight::SpotlightConfig>>,
    /// JSON Schema validator for LLM responses; None when disabled.
    pub schema: Option<Arc<crate::guard::schema::SchemaValidator>>,
    /// Tool gate for inspecting LLM-emitted tool calls; None when disabled.
    pub tool_gate: Option<Arc<crate::guard::tool_gate::ToolGate>>,
    /// Policy rule index — used by the audit layer to enrich match records
    /// with rule_id / category / severity. None when no policy bundle.
    pub policy: Option<Arc<crate::policy::PolicyRuleIndex>>,
    /// Resolved backend pool. Hot path: `pool.route(model)` picks
    /// the right backend for each request.
    pub pool: backend::BackendPoolRuntime,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
    pub audit: Option<Arc<audit::AuditLog>>,
    /// Client-auth runtime: SQLite-backed `client_tokens` table plus the
    /// in-memory verification cache. `None` when `[auth].enabled = false`
    /// AND the table was not opened at startup — today the table is
    /// opened whenever budget is enabled so the admin endpoints can
    /// issue tokens even before enforcement is turned on.
    pub client_auth: Option<client_auth::ClientAuth>,
}

impl AppState {
    /// Endpoint of the default backend in the live pool. Kept for
    /// log lines and the legacy `/v1/models` fallback that does not
    /// carry a request model. Hot-path callers should route by
    /// model via `state.pool.route(model)` instead.
    pub fn backend_endpoint(&self) -> &str {
        self.pool
            .get(self.pool.default_backend())
            .map(|b| b.endpoint())
            .unwrap_or("")
    }
}
