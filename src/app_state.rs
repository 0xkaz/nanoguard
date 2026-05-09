use std::sync::Arc;

use crate::{audit, backend, budget, config, matcher, proxy};

pub struct AppState {
    pub config: config::Config,
    pub matchers: Arc<matcher::Matchers>,
    pub redactor: Arc<proxy::redact::Redactor>,
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
