use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    #[serde(default)]
    pub nanoguard: ServerConfig,
    pub backend: BackendConfig,
    #[serde(default)]
    pub input: InputConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub audit: AuditConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    pub listen: String,
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:8080".to_string(),
            log_level: "info".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct BackendConfig {
    pub provider: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub model: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct InputConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub keyword: KeywordConfig,
    #[serde(default)]
    pub pii: PiiConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct OutputConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub pii: PiiConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct KeywordConfig {
    /// Keyword scan engine: "aho-corasick" (default) or "iword-rs"
    #[serde(default = "default_engine")]
    pub engine: String,
    pub dict_paths: Vec<String>,
    pub inline_block: Vec<String>,
    pub inline_alert: Vec<String>,
    pub inline_flag: Vec<String>,
}

fn default_engine() -> String {
    "aho-corasick".to_string()
}

impl Default for KeywordConfig {
    fn default() -> Self {
        Self {
            engine: default_engine(),
            dict_paths: vec![],
            inline_block: vec![
                "ignore previous instructions".to_string(),
                "disregard your instructions".to_string(),
                "jailbreak".to_string(),
                "dan mode".to_string(),
                "you are now".to_string(),
            ],
            inline_alert: vec![
                "password".to_string(),
                "api_key".to_string(),
                "secret".to_string(),
            ],
            inline_flag: vec![
                "bitcoin".to_string(),
                "crypto".to_string(),
                "gambling".to_string(),
            ],
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PiiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub action: PiiAction,
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            action: PiiAction::Mask,
        }
    }
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PiiAction {
    Mask,
    Reject,
    Log,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AuditConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_audit_path")]
    pub path: String,
    /// When true, only a SHA-256 hash of the prompt is logged (not the raw text)
    #[serde(default = "default_true")]
    pub hash_only: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_audit_path(),
            hash_only: true,
        }
    }
}

fn default_audit_path() -> String {
    "nanoguard-audit.jsonl".to_string()
}

#[derive(Debug, Deserialize, Clone)]
pub struct BudgetConfig {
    pub enabled: bool,
    pub db_path: String,
    /// Bearer token required for /v1/admin/* endpoints. None = admin API disabled.
    pub admin_api_key: Option<String>,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: "nanoguard.db".to_string(),
            admin_api_key: None,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {:?}", path.as_ref()))?;
        toml::from_str(&content).context("parsing config TOML")
    }

    pub fn from_env_or_default() -> Result<Self> {
        let path =
            std::env::var("NANOGUARD_CONFIG").unwrap_or_else(|_| "nanoguard.toml".to_string());
        if std::path::Path::new(&path).exists() {
            Self::from_file(&path)
        } else {
            // Minimal default: backend from env vars
            let provider =
                std::env::var("BACKEND_PROVIDER").unwrap_or_else(|_| "ollama".to_string());
            let endpoint = std::env::var("BACKEND_ENDPOINT")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            let api_key = std::env::var("BACKEND_API_KEY")
                .ok()
                .or_else(|| std::env::var("OPENAI_API_KEY").ok());
            let model = std::env::var("BACKEND_MODEL").ok();
            Ok(Config {
                nanoguard: ServerConfig::default(),
                backend: BackendConfig {
                    provider,
                    endpoint,
                    api_key,
                    model,
                },
                input: InputConfig::default(),
                output: OutputConfig::default(),
                budget: BudgetConfig {
                    admin_api_key: std::env::var("ADMIN_API_KEY").ok(),
                    ..BudgetConfig::default()
                },
                audit: AuditConfig::default(),
            })
        }
    }
}
