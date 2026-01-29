//! Trait interfaces for domain services the gateway delegates to.
//! Each trait has a `Noop` implementation that returns empty/default responses,
//! allowing the gateway to run standalone before domain crates are wired in.

use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// Error type returned by service methods.
pub type ServiceError = String;
pub type ServiceResult<T = Value> = Result<T, ServiceError>;

// ── Agent ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait AgentService: Send + Sync {
    async fn run(&self, params: Value) -> ServiceResult;
    async fn run_wait(&self, params: Value) -> ServiceResult;
    async fn identity_get(&self) -> ServiceResult;
    async fn list(&self) -> ServiceResult;
}

pub struct NoopAgentService;

#[async_trait]
impl AgentService for NoopAgentService {
    async fn run(&self, _params: Value) -> ServiceResult {
        Err("agent service not configured".into())
    }
    async fn run_wait(&self, _params: Value) -> ServiceResult {
        Err("agent service not configured".into())
    }
    async fn identity_get(&self) -> ServiceResult {
        Ok(serde_json::json!({ "name": "moltis", "avatar": null }))
    }
    async fn list(&self) -> ServiceResult {
        Ok(serde_json::json!([]))
    }
}

// ── Sessions ────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SessionService: Send + Sync {
    async fn list(&self) -> ServiceResult;
    async fn preview(&self, params: Value) -> ServiceResult;
    async fn resolve(&self, params: Value) -> ServiceResult;
    async fn patch(&self, params: Value) -> ServiceResult;
    async fn reset(&self, params: Value) -> ServiceResult;
    async fn delete(&self, params: Value) -> ServiceResult;
    async fn compact(&self, params: Value) -> ServiceResult;
}

pub struct NoopSessionService;

#[async_trait]
impl SessionService for NoopSessionService {
    async fn list(&self) -> ServiceResult { Ok(serde_json::json!([])) }
    async fn preview(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn resolve(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn patch(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn reset(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn delete(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn compact(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
}

// ── Channels ────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ChannelService: Send + Sync {
    async fn status(&self) -> ServiceResult;
    async fn logout(&self, params: Value) -> ServiceResult;
    async fn send(&self, params: Value) -> ServiceResult;
}

pub struct NoopChannelService;

#[async_trait]
impl ChannelService for NoopChannelService {
    async fn status(&self) -> ServiceResult { Ok(serde_json::json!({ "channels": [] })) }
    async fn logout(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn send(&self, _p: Value) -> ServiceResult {
        Err("no channels configured".into())
    }
}

// ── Config ──────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ConfigService: Send + Sync {
    async fn get(&self, params: Value) -> ServiceResult;
    async fn set(&self, params: Value) -> ServiceResult;
    async fn apply(&self, params: Value) -> ServiceResult;
    async fn patch(&self, params: Value) -> ServiceResult;
    async fn schema(&self) -> ServiceResult;
}

pub struct NoopConfigService;

#[async_trait]
impl ConfigService for NoopConfigService {
    async fn get(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn set(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn apply(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn patch(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn schema(&self) -> ServiceResult { Ok(serde_json::json!({})) }
}

// ── Cron ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait CronService: Send + Sync {
    async fn list(&self) -> ServiceResult;
    async fn status(&self) -> ServiceResult;
    async fn add(&self, params: Value) -> ServiceResult;
    async fn update(&self, params: Value) -> ServiceResult;
    async fn remove(&self, params: Value) -> ServiceResult;
    async fn run(&self, params: Value) -> ServiceResult;
    async fn runs(&self, params: Value) -> ServiceResult;
}

pub struct NoopCronService;

#[async_trait]
impl CronService for NoopCronService {
    async fn list(&self) -> ServiceResult { Ok(serde_json::json!([])) }
    async fn status(&self) -> ServiceResult { Ok(serde_json::json!({ "running": false })) }
    async fn add(&self, _p: Value) -> ServiceResult { Err("cron not configured".into()) }
    async fn update(&self, _p: Value) -> ServiceResult { Err("cron not configured".into()) }
    async fn remove(&self, _p: Value) -> ServiceResult { Err("cron not configured".into()) }
    async fn run(&self, _p: Value) -> ServiceResult { Err("cron not configured".into()) }
    async fn runs(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!([])) }
}

// ── Chat ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ChatService: Send + Sync {
    async fn send(&self, params: Value) -> ServiceResult;
    async fn abort(&self, params: Value) -> ServiceResult;
    async fn history(&self, params: Value) -> ServiceResult;
    async fn inject(&self, params: Value) -> ServiceResult;
}

pub struct NoopChatService;

#[async_trait]
impl ChatService for NoopChatService {
    async fn send(&self, _p: Value) -> ServiceResult { Err("chat not configured".into()) }
    async fn abort(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn history(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!([])) }
    async fn inject(&self, _p: Value) -> ServiceResult { Err("chat not configured".into()) }
}

// ── TTS ─────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait TtsService: Send + Sync {
    async fn status(&self) -> ServiceResult;
    async fn providers(&self) -> ServiceResult;
    async fn enable(&self, params: Value) -> ServiceResult;
    async fn disable(&self) -> ServiceResult;
    async fn convert(&self, params: Value) -> ServiceResult;
    async fn set_provider(&self, params: Value) -> ServiceResult;
}

pub struct NoopTtsService;

#[async_trait]
impl TtsService for NoopTtsService {
    async fn status(&self) -> ServiceResult { Ok(serde_json::json!({ "enabled": false })) }
    async fn providers(&self) -> ServiceResult { Ok(serde_json::json!([])) }
    async fn enable(&self, _p: Value) -> ServiceResult { Err("tts not available".into()) }
    async fn disable(&self) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn convert(&self, _p: Value) -> ServiceResult { Err("tts not available".into()) }
    async fn set_provider(&self, _p: Value) -> ServiceResult { Err("tts not available".into()) }
}

// ── Skills ──────────────────────────────────────────────────────────────────

#[async_trait]
pub trait SkillsService: Send + Sync {
    async fn status(&self) -> ServiceResult;
    async fn bins(&self) -> ServiceResult;
    async fn install(&self, params: Value) -> ServiceResult;
    async fn update(&self, params: Value) -> ServiceResult;
}

pub struct NoopSkillsService;

#[async_trait]
impl SkillsService for NoopSkillsService {
    async fn status(&self) -> ServiceResult { Ok(serde_json::json!({ "installed": [] })) }
    async fn bins(&self) -> ServiceResult { Ok(serde_json::json!([])) }
    async fn install(&self, _p: Value) -> ServiceResult { Err("skills not available".into()) }
    async fn update(&self, _p: Value) -> ServiceResult { Err("skills not available".into()) }
}

// ── Browser ─────────────────────────────────────────────────────────────────

#[async_trait]
pub trait BrowserService: Send + Sync {
    async fn request(&self, params: Value) -> ServiceResult;
}

pub struct NoopBrowserService;

#[async_trait]
impl BrowserService for NoopBrowserService {
    async fn request(&self, _p: Value) -> ServiceResult { Err("browser not available".into()) }
}

// ── Usage ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait UsageService: Send + Sync {
    async fn status(&self) -> ServiceResult;
    async fn cost(&self, params: Value) -> ServiceResult;
}

pub struct NoopUsageService;

#[async_trait]
impl UsageService for NoopUsageService {
    async fn status(&self) -> ServiceResult {
        Ok(serde_json::json!({ "totalCost": 0, "requests": 0 }))
    }
    async fn cost(&self, _p: Value) -> ServiceResult {
        Ok(serde_json::json!({ "cost": 0 }))
    }
}

// ── Exec Approvals ──────────────────────────────────────────────────────────

#[async_trait]
pub trait ExecApprovalService: Send + Sync {
    async fn get(&self) -> ServiceResult;
    async fn set(&self, params: Value) -> ServiceResult;
    async fn node_get(&self, params: Value) -> ServiceResult;
    async fn node_set(&self, params: Value) -> ServiceResult;
    async fn request(&self, params: Value) -> ServiceResult;
    async fn resolve(&self, params: Value) -> ServiceResult;
}

pub struct NoopExecApprovalService;

#[async_trait]
impl ExecApprovalService for NoopExecApprovalService {
    async fn get(&self) -> ServiceResult { Ok(serde_json::json!({ "mode": "always" })) }
    async fn set(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn node_get(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({ "mode": "always" })) }
    async fn node_set(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn request(&self, _p: Value) -> ServiceResult { Err("approvals not configured".into()) }
    async fn resolve(&self, _p: Value) -> ServiceResult { Err("approvals not configured".into()) }
}

// ── Onboarding ──────────────────────────────────────────────────────────────

#[async_trait]
pub trait OnboardingService: Send + Sync {
    async fn wizard_start(&self, params: Value) -> ServiceResult;
    async fn wizard_next(&self, params: Value) -> ServiceResult;
    async fn wizard_cancel(&self) -> ServiceResult;
    async fn wizard_status(&self) -> ServiceResult;
}

pub struct NoopOnboardingService;

#[async_trait]
impl OnboardingService for NoopOnboardingService {
    async fn wizard_start(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({ "step": 0 })) }
    async fn wizard_next(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({ "step": 0, "done": true })) }
    async fn wizard_cancel(&self) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn wizard_status(&self) -> ServiceResult { Ok(serde_json::json!({ "active": false })) }
}

// ── Update ──────────────────────────────────────────────────────────────────

#[async_trait]
pub trait UpdateService: Send + Sync {
    async fn run(&self, params: Value) -> ServiceResult;
}

pub struct NoopUpdateService;

#[async_trait]
impl UpdateService for NoopUpdateService {
    async fn run(&self, _p: Value) -> ServiceResult { Err("update not available".into()) }
}

// ── Model ───────────────────────────────────────────────────────────────────

#[async_trait]
pub trait ModelService: Send + Sync {
    async fn list(&self) -> ServiceResult;
}

pub struct NoopModelService;

#[async_trait]
impl ModelService for NoopModelService {
    async fn list(&self) -> ServiceResult { Ok(serde_json::json!([])) }
}

// ── Web Login ───────────────────────────────────────────────────────────────

#[async_trait]
pub trait WebLoginService: Send + Sync {
    async fn start(&self, params: Value) -> ServiceResult;
    async fn wait(&self, params: Value) -> ServiceResult;
}

pub struct NoopWebLoginService;

#[async_trait]
impl WebLoginService for NoopWebLoginService {
    async fn start(&self, _p: Value) -> ServiceResult { Err("web login not available".into()) }
    async fn wait(&self, _p: Value) -> ServiceResult { Err("web login not available".into()) }
}

// ── Voicewake ───────────────────────────────────────────────────────────────

#[async_trait]
pub trait VoicewakeService: Send + Sync {
    async fn get(&self) -> ServiceResult;
    async fn set(&self, params: Value) -> ServiceResult;
    async fn wake(&self, params: Value) -> ServiceResult;
    async fn talk_mode(&self, params: Value) -> ServiceResult;
}

pub struct NoopVoicewakeService;

#[async_trait]
impl VoicewakeService for NoopVoicewakeService {
    async fn get(&self) -> ServiceResult { Ok(serde_json::json!({ "enabled": false })) }
    async fn set(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
    async fn wake(&self, _p: Value) -> ServiceResult { Err("voicewake not available".into()) }
    async fn talk_mode(&self, _p: Value) -> ServiceResult { Ok(serde_json::json!({})) }
}

// ── Logs ────────────────────────────────────────────────────────────────────

#[async_trait]
pub trait LogsService: Send + Sync {
    async fn tail(&self, params: Value) -> ServiceResult;
}

pub struct NoopLogsService;

#[async_trait]
impl LogsService for NoopLogsService {
    async fn tail(&self, _p: Value) -> ServiceResult {
        Ok(serde_json::json!({ "subscribed": true }))
    }
}

// ── Bundled services ────────────────────────────────────────────────────────

/// All domain services the gateway delegates to.
pub struct GatewayServices {
    pub agent: Arc<dyn AgentService>,
    pub session: Arc<dyn SessionService>,
    pub channel: Arc<dyn ChannelService>,
    pub config: Arc<dyn ConfigService>,
    pub cron: Arc<dyn CronService>,
    pub chat: Arc<dyn ChatService>,
    pub tts: Arc<dyn TtsService>,
    pub skills: Arc<dyn SkillsService>,
    pub browser: Arc<dyn BrowserService>,
    pub usage: Arc<dyn UsageService>,
    pub exec_approval: Arc<dyn ExecApprovalService>,
    pub onboarding: Arc<dyn OnboardingService>,
    pub update: Arc<dyn UpdateService>,
    pub model: Arc<dyn ModelService>,
    pub web_login: Arc<dyn WebLoginService>,
    pub voicewake: Arc<dyn VoicewakeService>,
    pub logs: Arc<dyn LogsService>,
}

impl GatewayServices {
    pub fn with_chat(mut self, chat: Arc<dyn ChatService>) -> Self {
        self.chat = chat;
        self
    }

    pub fn with_model(mut self, model: Arc<dyn ModelService>) -> Self {
        self.model = model;
        self
    }

    /// Create a service bundle with all noop implementations.
    pub fn noop() -> Self {
        Self {
            agent: Arc::new(NoopAgentService),
            session: Arc::new(NoopSessionService),
            channel: Arc::new(NoopChannelService),
            config: Arc::new(NoopConfigService),
            cron: Arc::new(NoopCronService),
            chat: Arc::new(NoopChatService),
            tts: Arc::new(NoopTtsService),
            skills: Arc::new(NoopSkillsService),
            browser: Arc::new(NoopBrowserService),
            usage: Arc::new(NoopUsageService),
            exec_approval: Arc::new(NoopExecApprovalService),
            onboarding: Arc::new(NoopOnboardingService),
            update: Arc::new(NoopUpdateService),
            model: Arc::new(NoopModelService),
            web_login: Arc::new(NoopWebLoginService),
            voicewake: Arc::new(NoopVoicewakeService),
            logs: Arc::new(NoopLogsService),
        }
    }
}
