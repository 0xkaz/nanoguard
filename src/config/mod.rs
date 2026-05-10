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
    /// Shadow mode: scan + audit but do not actually block. Useful when rolling
    /// out new rules — observe what would have been blocked before enforcing.
    #[serde(default)]
    pub shadow: bool,
    #[serde(default)]
    pub keyword: KeywordConfig,
    #[serde(default)]
    pub pii: PiiConfig,
    /// Spotlighting: tag retrieved/tool content so the LLM treats it as data,
    /// not as instructions. Defends against indirect prompt injection.
    #[serde(default)]
    pub spotlight: SpotlightConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct SpotlightConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "delimiting" | "datamarking" | "encoding". Default "datamarking".
    #[serde(default = "default_spotlight_method")]
    pub method: String,
    /// Roles whose content is treated as untrusted. Default: ["tool"].
    #[serde(default = "default_untrusted_roles")]
    pub untrusted_roles: Vec<String>,
    #[serde(default = "default_delim_open")]
    pub delimiter_open: String,
    #[serde(default = "default_delim_close")]
    pub delimiter_close: String,
    #[serde(default = "default_datamark")]
    pub datamark_char: String,
    /// System rider injected to teach the model the convention. Empty string
    /// to disable the rider entirely (not recommended).
    #[serde(default)]
    pub system_rider: Option<String>,
}

fn default_spotlight_method() -> String {
    "datamarking".to_string()
}
fn default_untrusted_roles() -> Vec<String> {
    vec!["tool".to_string()]
}
fn default_delim_open() -> String {
    "<<UNTRUSTED>>".to_string()
}
fn default_delim_close() -> String {
    "<</UNTRUSTED>>".to_string()
}
fn default_datamark() -> String {
    "^".to_string()
}

impl Default for SpotlightConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: default_spotlight_method(),
            untrusted_roles: default_untrusted_roles(),
            delimiter_open: default_delim_open(),
            delimiter_close: default_delim_close(),
            datamark_char: default_datamark(),
            system_rider: None,
        }
    }
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
    #[serde(default)]
    pub normalize: NormalizeConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NormalizeConfig {
    /// NFKC Unicode normalization (full-width → half-width, combining chars). Default: true.
    #[serde(default = "default_true")]
    pub nfkc: bool,
    /// Strip zero-width chars (U+200B/C/D, U+FEFF). Default: true.
    #[serde(default = "default_true")]
    pub zero_width: bool,
    /// Collapse single-char separators ("j-a-i-l" → "jail"). Default: false (off).
    #[serde(default)]
    pub separators: bool,
    /// Leet-speak fold (3→e, 0→o, 1→i, 4→a, 5→s, 7→t, @→a). Default: false (off).
    #[serde(default)]
    pub leet: bool,
}

impl Default for NormalizeConfig {
    fn default() -> Self {
        Self {
            nfkc: true,
            zero_width: true,
            separators: false,
            leet: false,
        }
    }
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
            normalize: NormalizeConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct PiiConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub action: PiiAction,
    /// Optional dictionary files in `/regex/<TAB>ENTITY_NAME` form. Loaded on
    /// top of the built-in entity set; appears as `[ENTITY_NAME]` placeholders.
    #[serde(default)]
    pub dict_paths: Vec<String>,
    /// Placeholder format. "bare" → `[EMAIL]`, "indexed" → `[EMAIL_1]`,
    /// "llm_guard" → `[REDACTED_EMAIL_1]`. Indexed forms preserve information
    /// when the same prompt has multiple distinct values of one entity type
    /// and are required for future Vault-backed deanonymization.
    #[serde(default = "default_placeholder_style")]
    pub placeholder_style: String,
    /// Per-entity action overrides. Keys are entity names (e.g. EMAIL,
    /// AWS_ACCESS_KEY_ID). Values are "mask" / "reject" / "log". Entities
    /// not listed here use the global `action`.
    #[serde(default)]
    pub entities: std::collections::HashMap<String, PiiAction>,
    /// Reversible redaction: when true, mask-class matches are recorded in a
    /// per-request Vault and restored from the LLM response, so the model
    /// never sees the originals but the client gets unmasked output.
    /// Implies an indexed placeholder style for safe round-trips.
    #[serde(default)]
    pub reversible: bool,
    /// Deanonymize matching strategy when `reversible = true`. One of
    /// "exact" (default) / "case_insensitive". "fuzzy" / "combined" are
    /// reserved for future implementation and currently fall back to exact.
    #[serde(default = "default_deanon_strategy")]
    pub deanonymize_strategy: String,
}

fn default_deanon_strategy() -> String {
    "exact".to_string()
}

fn default_placeholder_style() -> String {
    "bare".to_string()
}

impl Default for PiiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            action: PiiAction::Mask,
            dict_paths: vec![],
            placeholder_style: default_placeholder_style(),
            entities: std::collections::HashMap::new(),
            reversible: false,
            deanonymize_strategy: default_deanon_strategy(),
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
