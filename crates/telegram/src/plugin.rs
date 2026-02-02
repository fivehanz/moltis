use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Instant,
};

use {
    anyhow::Result,
    async_trait::async_trait,
    secrecy::ExposeSecret,
    teloxide::prelude::Requester,
    tracing::{info, warn},
};

use moltis_channels::{
    ChannelEventSink,
    message_log::MessageLog,
    plugin::{ChannelHealthSnapshot, ChannelOutbound, ChannelPlugin, ChannelStatus},
};

use crate::{
    bot, config::TelegramAccountConfig, outbound::TelegramOutbound, state::AccountStateMap,
};

/// Cache TTL for probe results (30 seconds).
const PROBE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// Telegram channel plugin.
pub struct TelegramPlugin {
    accounts: AccountStateMap,
    outbound: TelegramOutbound,
    message_log: Option<Arc<dyn MessageLog>>,
    event_sink: Option<Arc<dyn ChannelEventSink>>,
    probe_cache: RwLock<HashMap<String, (ChannelHealthSnapshot, Instant)>>,
}

impl TelegramPlugin {
    pub fn new() -> Self {
        let accounts: AccountStateMap = Arc::new(RwLock::new(HashMap::new()));
        let outbound = TelegramOutbound {
            accounts: Arc::clone(&accounts),
        };
        Self {
            accounts,
            outbound,
            message_log: None,
            event_sink: None,
            probe_cache: RwLock::new(HashMap::new()),
        }
    }

    pub fn with_message_log(mut self, log: Arc<dyn MessageLog>) -> Self {
        self.message_log = Some(log);
        self
    }

    pub fn with_event_sink(mut self, sink: Arc<dyn ChannelEventSink>) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Get a shared reference to the outbound sender (for use outside the plugin).
    pub fn shared_outbound(&self) -> Arc<dyn moltis_channels::ChannelOutbound> {
        Arc::new(TelegramOutbound {
            accounts: Arc::clone(&self.accounts),
        })
    }

    /// List all active account IDs.
    pub fn account_ids(&self) -> Vec<String> {
        let accounts = self.accounts.read().unwrap();
        accounts.keys().cloned().collect()
    }

    /// Get the config for a specific account (serialized to JSON).
    pub fn account_config(&self, account_id: &str) -> Option<serde_json::Value> {
        let accounts = self.accounts.read().unwrap();
        accounts
            .get(account_id)
            .and_then(|s| serde_json::to_value(&s.config).ok())
    }
}

impl Default for TelegramPlugin {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ChannelPlugin for TelegramPlugin {
    fn id(&self) -> &str {
        "telegram"
    }

    fn name(&self) -> &str {
        "Telegram"
    }

    async fn start_account(&mut self, account_id: &str, config: serde_json::Value) -> Result<()> {
        let tg_config: TelegramAccountConfig = serde_json::from_value(config)?;

        if tg_config.token.expose_secret().is_empty() {
            return Err(anyhow::anyhow!("telegram bot token is required"));
        }

        info!(account_id, "starting telegram account");

        bot::start_polling(
            account_id.to_string(),
            tg_config,
            Arc::clone(&self.accounts),
            self.message_log.clone(),
            self.event_sink.clone(),
        )
        .await?;

        Ok(())
    }

    async fn stop_account(&mut self, account_id: &str) -> Result<()> {
        let cancel = {
            let accounts = self.accounts.read().unwrap();
            accounts.get(account_id).map(|s| s.cancel.clone())
        };

        if let Some(cancel) = cancel {
            info!(account_id, "stopping telegram account");
            cancel.cancel();
            let mut accounts = self.accounts.write().unwrap();
            accounts.remove(account_id);
        } else {
            warn!(account_id, "telegram account not found");
        }

        Ok(())
    }

    fn outbound(&self) -> Option<&dyn ChannelOutbound> {
        Some(&self.outbound)
    }

    fn status(&self) -> Option<&dyn ChannelStatus> {
        Some(self)
    }
}

#[async_trait]
impl ChannelStatus for TelegramPlugin {
    async fn probe(&self, account_id: &str) -> Result<ChannelHealthSnapshot> {
        // Return cached result if fresh enough.
        if let Ok(cache) = self.probe_cache.read()
            && let Some((snap, ts)) = cache.get(account_id)
            && ts.elapsed() < PROBE_CACHE_TTL
        {
            return Ok(snap.clone());
        }

        let bot = {
            let accounts = self.accounts.read().unwrap();
            accounts.get(account_id).map(|s| s.bot.clone())
        };

        let result = match bot {
            Some(bot) => match bot.get_me().await {
                Ok(me) => ChannelHealthSnapshot {
                    connected: true,
                    account_id: account_id.to_string(),
                    details: Some(format!(
                        "Bot: @{}",
                        me.username.as_deref().unwrap_or("unknown")
                    )),
                },
                Err(e) => ChannelHealthSnapshot {
                    connected: false,
                    account_id: account_id.to_string(),
                    details: Some(format!("API error: {e}")),
                },
            },
            None => ChannelHealthSnapshot {
                connected: false,
                account_id: account_id.to_string(),
                details: Some("account not started".into()),
            },
        };

        if let Ok(mut cache) = self.probe_cache.write() {
            cache.insert(account_id.to_string(), (result.clone(), Instant::now()));
        }

        Ok(result)
    }
}
