use std::sync::Arc;

use crate::{audit, backend, budget, config, matcher, proxy};

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
    pub backend: backend::Backend,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
    pub audit: Option<Arc<audit::AuditLog>>,
}

impl AppState {
    pub fn backend_endpoint(&self) -> &str {
        self.config.backend.endpoint.trim_end_matches('/')
    }
}
