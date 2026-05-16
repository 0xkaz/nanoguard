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
    #[serde(default)]
    pub tools: ToolsConfig,
    #[serde(default)]
    pub policies: PoliciesConfig,
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(default)]
    pub console: ConsoleConfig,
    #[serde(default)]
    pub reload: ReloadConfig,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct PoliciesConfig {
    /// Optional path to a YAML policy bundle. Empty string = disabled.
    /// When set, the rules in the bundle are merged into the existing
    /// inline keyword and PII redactor rules at startup.
    #[serde(default)]
    pub bundle_path: Option<String>,
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
    #[serde(default)]
    pub schema: OutputSchemaConfig,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputSchemaConfig {
    #[serde(default)]
    pub enabled: bool,
    /// "reject" | "log" | "repair". Reserved values map to LogOnly until
    /// implemented.
    #[serde(default = "default_violation_action")]
    pub on_violation: String,
    /// One or more rules to apply per (endpoint, model) tuple.
    #[serde(default)]
    pub rules: Vec<OutputSchemaRule>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputSchemaRule {
    pub endpoint: String,
    /// Optional regex matched against the request `model` field. Empty / absent
    /// means "any model".
    #[serde(default)]
    pub model_pattern: Option<String>,
    /// Filesystem path to a JSON Schema file (Draft 2020-12).
    pub schema_path: String,
    /// Optional human-readable name for logs.
    #[serde(default)]
    pub name: Option<String>,
}

fn default_violation_action() -> String {
    "log".to_string()
}

impl Default for OutputSchemaConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            on_violation: default_violation_action(),
            rules: vec![],
        }
    }
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

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Optional allow list. If set, only matching tools pass. Wildcards (`*`)
    /// supported as prefix or suffix.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    #[serde(default)]
    pub deny: Vec<String>,
    /// Per-tool JSON Schema specs.
    #[serde(default)]
    pub schemas: Vec<ToolSchemaSpec>,
    /// Reject tool calls whose argument JSON contains any of these entity
    /// names (matched against the request-side Redactor's rules).
    #[serde(default)]
    pub reject_entities: Vec<String>,
    /// Mask matching entities in tool arguments instead of rejecting.
    #[serde(default)]
    pub mask_entities: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ToolSchemaSpec {
    pub tool_name: String,
    pub schema_path: String,
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
    /// When true, call `fsync` after every audit write. Trades throughput
    /// for crash-durability of the most recent entries. Default `false`:
    /// the audit log is best-effort and most deployments accept the small
    /// loss window in exchange for not blocking the hot path on disk
    /// flushes.
    #[serde(default)]
    pub fsync_every_write: bool,
}

impl Default for AuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_audit_path(),
            hash_only: true,
            fsync_every_write: false,
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

/// Client-token authentication for the proxy endpoints.
///
/// Stage 1 of the `docs/design/client-auth.md` rollout: `enabled = false`
/// by default. Even when on, the verification middleware lives behind
/// this gate so existing deployments are not broken by the mere
/// presence of the feature.
#[derive(Debug, Deserialize, Clone)]
pub struct AuthConfig {
    /// Master switch for the client-token verification middleware.
    /// When false, the middleware short-circuits to "allow" without
    /// looking at the Authorization header.
    #[serde(default)]
    pub enabled: bool,

    /// Single character embedded in the wire token shape (`ng_<env>_…`).
    /// `p` for production, `t` for test/dev. Surfaces in audit logs so
    /// operators can tell at a glance which environment a leaked token
    /// belongs to.
    #[serde(default = "default_env_marker")]
    pub env_marker: char,

    /// Upper bound on the in-memory token cache. Beyond this, the
    /// cache evicts LRU.
    #[serde(default = "default_cache_capacity")]
    pub cache_capacity: usize,

    /// TTL for cached `ClientView` entries. After this the next request
    /// re-reads from SQLite. Bounds the maximum staleness window for
    /// revocations that lose the broadcast invalidation.
    #[serde(default = "default_cache_ttl_secs")]
    pub cache_ttl_secs: u64,

    /// When true and the proxy is not on loopback, requests are refused
    /// unless `X-Forwarded-Proto: https` is present. See the design doc
    /// `client-auth.md > Transport`.
    #[serde(default)]
    pub require_https: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            env_marker: 'p',
            cache_capacity: 10_000,
            cache_ttl_secs: 60,
            require_https: false,
        }
    }
}

fn default_env_marker() -> char {
    'p'
}

fn default_cache_capacity() -> usize {
    10_000
}

fn default_cache_ttl_secs() -> u64 {
    60
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
pub struct ConsoleConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub session_secret: String,
    #[serde(default = "default_session_ttl_hours")]
    pub session_ttl_hours: i64,
    #[serde(default = "default_console_audit_path")]
    pub audit_path: String,
    #[serde(default)]
    pub auth: ConsoleAuthConfig,
}

fn default_listen() -> String {
    "127.0.0.1:8081".to_string()
}

impl ConsoleConfig {
    pub fn default_listen() -> String {
        default_listen()
    }
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            listen: Self::default_listen(),
            session_secret: String::new(),
            session_ttl_hours: default_session_ttl_hours(),
            audit_path: default_console_audit_path(),
            auth: ConsoleAuthConfig::default(),
        }
    }
}

fn default_session_ttl_hours() -> i64 {
    24
}

fn default_console_audit_path() -> String {
    "console-audit.jsonl".to_string()
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ReloadConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Path to a PID file written by the proxy. The console reads this
    /// and sends SIGHUP to trigger reload.
    #[serde(default)]
    pub pid_file: Option<String>,
    /// Optional Unix domain socket path for reload IPC.
    #[serde(default)]
    pub socket: Option<String>,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ConsoleAuthConfig {
    #[serde(default = "default_console_auth_mode")]
    pub mode: String,
    #[serde(default)]
    pub local: ConsoleLocalAuthConfig,
    /// Reserved for future OIDC support (Phase 4). Present so TOML with
    /// `[console.auth.oidc]` does not fail to parse.
    #[serde(default)]
    pub oidc: Option<toml::Value>,
}

fn default_console_auth_mode() -> String {
    "local".to_string()
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct ConsoleLocalAuthConfig {
    #[serde(default)]
    pub allow_signup: bool,
    pub bootstrap_admin: Option<BootstrapAdminConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct BootstrapAdminConfig {
    pub username: String,
    pub password_env: String,
}

impl Config {
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading config {:?}", path.as_ref()))?;
        Self::from_file_content(&content)
    }

    pub fn from_file_content(content: &str) -> Result<Self> {
        toml::from_str(content).context("parsing config TOML")
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
                tools: ToolsConfig::default(),
                policies: PoliciesConfig::default(),
                auth: AuthConfig::default(),
                reload: ReloadConfig::default(),
                console: ConsoleConfig::default(),
            })
        }
    }
}
