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
}
