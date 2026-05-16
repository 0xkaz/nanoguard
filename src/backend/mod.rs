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
}
