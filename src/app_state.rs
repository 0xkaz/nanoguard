use std::sync::Arc;

use crate::{backend, budget, config, matcher};

pub struct AppState {
    pub config: config::Config,
    pub matchers: matcher::Matchers,
    pub backend: backend::Backend,
    pub http_client: reqwest::Client,
    pub budget: Option<Arc<dyn budget::BudgetStore>>,
}

impl AppState {
    pub fn backend_endpoint(&self) -> &str {
        self.config.backend.endpoint.trim_end_matches('/')
    }
}
