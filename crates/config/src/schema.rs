/// Config schema types (agents, channels, tools, session, gateway, plugins).
/// Corresponds to `src/config/types.ts` and `zod-schema.*.ts` in the TS codebase.
use std::collections::HashMap;

use {
    secrecy::{ExposeSecret, Secret},
    serde::{Deserialize, Serialize},
};

/// Agent identity (name, emoji, creature, vibe, soul).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentIdentity {
    pub name: Option<String>,
    pub emoji: Option<String>,
    pub creature: Option<String>,
    pub vibe: Option<String>,
    /// Freeform personality / soul text injected into the system prompt.
    pub soul: Option<String>,
}

/// User profile collected during onboarding.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UserProfile {
    pub name: Option<String>,
    pub timezone: Option<String>,
}

/// Resolved identity combining agent identity and user profile.
/// Used as the API response for `identity_get` and in the gon data blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedIdentity {
    pub name: String,
    pub emoji: Option<String>,
    pub creature: Option<String>,
    pub vibe: Option<String>,
    pub soul: Option<String>,
    pub user_name: Option<String>,
}

impl ResolvedIdentity {
    pub fn from_config(cfg: &MoltisConfig) -> Self {
        Self {
            name: cfg.identity.name.clone().unwrap_or_else(|| "moltis".into()),
            emoji: cfg.identity.emoji.clone(),
            creature: cfg.identity.creature.clone(),
            vibe: cfg.identity.vibe.clone(),
            soul: cfg.identity.soul.clone(),
            user_name: cfg.user.name.clone(),
        }
    }
}

impl Default for ResolvedIdentity {
    fn default() -> Self {
        Self {
            name: "moltis".into(),
            emoji: None,
            creature: None,
            vibe: None,
            soul: None,
            user_name: None,
        }
    }
}

/// Root configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MoltisConfig {
    pub providers: ProvidersConfig,
    pub tools: ToolsConfig,
    pub skills: SkillsConfig,
    pub channels: ChannelsConfig,
    pub tls: TlsConfig,
    pub auth: AuthConfig,
    pub identity: AgentIdentity,
    pub user: UserProfile,
}

/// Authentication configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthConfig {
    /// When true, authentication is explicitly disabled (no login required).
    pub disabled: bool,
}

impl MoltisConfig {
    /// Returns `true` when both the agent name and user name have been set
    /// (i.e. the onboarding wizard has been completed).
    pub fn is_onboarded(&self) -> bool {
        self.identity.name.is_some() && self.user.name.is_some()
    }
}

/// Skills configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Whether the skills system is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Extra directories to search for skills.
    #[serde(default)]
    pub search_paths: Vec<String>,
    /// Skills to always load (by name) without explicit activation.
    #[serde(default)]
    pub auto_load: Vec<String>,
}

fn default_true() -> bool {
    true
}

/// Channel configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    /// Telegram bot accounts, keyed by account ID.
    #[serde(default)]
    pub telegram: HashMap<String, serde_json::Value>,
}

/// TLS configuration for the gateway HTTPS server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TlsConfig {
    /// Enable HTTPS with auto-generated certificates. Defaults to true.
    pub enabled: bool,
    /// Auto-generate a local CA and server certificate on first run.
    pub auto_generate: bool,
    /// Path to a custom server certificate (PEM). Overrides auto-generation.
    pub cert_path: Option<String>,
    /// Path to a custom server private key (PEM). Overrides auto-generation.
    pub key_path: Option<String>,
    /// Path to the CA certificate (PEM) used for trust instructions.
    pub ca_cert_path: Option<String>,
    /// Port for the plain-HTTP redirect/CA-download server.
    pub http_redirect_port: Option<u16>,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_generate: true,
            cert_path: None,
            key_path: None,
            ca_cert_path: None,
            http_redirect_port: Some(18790),
        }
    }
}

/// Tools configuration (exec, sandbox, policy).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub exec: ExecConfig,
    pub policy: ToolPolicyConfig,
}

/// Exec tool configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecConfig {
    pub default_timeout_secs: u64,
    pub max_output_bytes: usize,
    pub approval_mode: String,
    pub security_level: String,
    pub allowlist: Vec<String>,
    pub sandbox: SandboxConfig,
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self {
            default_timeout_secs: 30,
            max_output_bytes: 200 * 1024,
            approval_mode: "on-miss".into(),
            security_level: "allowlist".into(),
            allowlist: Vec::new(),
            sandbox: SandboxConfig::default(),
        }
    }
}

/// Resource limits for sandboxed execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceLimitsConfig {
    /// Memory limit (e.g. "512M", "1G").
    pub memory_limit: Option<String>,
    /// CPU quota as a fraction (e.g. 0.5 = half a core, 2.0 = two cores).
    pub cpu_quota: Option<f64>,
    /// Maximum number of PIDs.
    pub pids_max: Option<u32>,
}

/// Sandbox configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub mode: String,
    pub scope: String,
    pub workspace_mount: String,
    pub image: Option<String>,
    pub container_prefix: Option<String>,
    pub no_network: bool,
    /// Backend: "auto" (default), "docker", or "apple-container".
    /// "auto" prefers Apple Container on macOS when available, falls back to Docker.
    pub backend: String,
    pub resource_limits: ResourceLimitsConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            mode: "all".into(),
            scope: "session".into(),
            workspace_mount: "ro".into(),
            image: None,
            container_prefix: None,
            no_network: true,
            backend: "auto".into(),
            resource_limits: ResourceLimitsConfig::default(),
        }
    }
}

/// Tool policy configuration (allow/deny lists).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolPolicyConfig {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub profile: Option<String>,
}

/// OAuth provider configuration (e.g. openai-codex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthProviderConfig {
    pub client_id: String,
    pub auth_url: String,
    pub token_url: String,
    pub redirect_uri: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub callback_port: u16,
}

/// LLM provider configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    /// Provider-specific settings keyed by provider name.
    /// Known keys: "anthropic", "openai", "gemini", "groq", "xai", "deepseek"
    #[serde(flatten)]
    pub providers: HashMap<String, ProviderEntry>,
}

/// Configuration for a single LLM provider.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderEntry {
    /// Whether this provider is enabled. Defaults to true.
    pub enabled: bool,

    /// Override the API key (optional; env var still takes precedence if set).
    #[serde(
        default,
        serialize_with = "serialize_option_secret",
        skip_serializing_if = "Option::is_none"
    )]
    pub api_key: Option<Secret<String>>,

    /// Override the base URL.
    pub base_url: Option<String>,

    /// Default model ID for this provider.
    pub model: Option<String>,
}

impl std::fmt::Debug for ProviderEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderEntry")
            .field("enabled", &self.enabled)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

impl Default for ProviderEntry {
    fn default() -> Self {
        Self {
            enabled: true,
            api_key: None,
            base_url: None,
            model: None,
        }
    }
}

// ── Serde helpers for Secret<String> ────────────────────────────────────────

fn serialize_option_secret<S: serde::Serializer>(
    secret: &Option<Secret<String>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match secret {
        Some(s) => serializer.serialize_some(s.expose_secret()),
        None => serializer.serialize_none(),
    }
}

impl ProvidersConfig {
    /// Check if a provider is enabled (defaults to true if not configured).
    pub fn is_enabled(&self, name: &str) -> bool {
        self.providers.get(name).is_none_or(|e| e.enabled)
    }

    /// Get the configured entry for a provider, if any.
    pub fn get(&self, name: &str) -> Option<&ProviderEntry> {
        self.providers.get(name)
    }
}
