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
    pub backend: backend::Backend,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
    pub audit: Option<Arc<audit::AuditLog>>,
    /// Client-auth runtime: SQLite-backed `client_tokens` table and (in a
    /// later commit) the in-memory verification cache. `None` when
    /// `[auth].enabled = false` AND the table was not opened at startup —
    /// today the table is opened whenever budget is enabled so the admin
    /// endpoints can issue tokens even before enforcement is turned on.
    pub client_auth: Option<client_auth::ClientAuth>,
}

impl AppState {
    /// The backend endpoint this `AppState` forwards requests to.
    ///
    /// Reads from `self.backend` (preserved across hot reload), NOT
    /// from `self.config.backend` (which is the freshly-reloaded TOML
    /// view and may diverge from what the connection pool was built
    /// against). Anything externally visible — `/v1/models`, logs,
    /// future health checks — must read this method to stay consistent
    /// with the writes `forward_chat` actually performs.
    pub fn backend_endpoint(&self) -> &str {
        self.backend.endpoint()
    }
}
