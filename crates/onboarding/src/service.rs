//! Live onboarding service that backs the `wizard.*` RPC methods.

use std::{path::PathBuf, sync::Mutex};

use serde_json::{Value, json};

use moltis_config::MoltisConfig;

use crate::state::{WizardState, WizardStep};

/// Live onboarding service backed by a `WizardState` and config persistence.
pub struct LiveOnboardingService {
    state: Mutex<Option<WizardState>>,
    config_path: PathBuf,
}

impl LiveOnboardingService {
    pub fn new(config_path: PathBuf) -> Self {
        Self {
            state: Mutex::new(None),
            config_path,
        }
    }

    /// Save config to the service's config path.
    fn save(&self, config: &MoltisConfig) -> anyhow::Result<()> {
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let toml_str =
            toml::to_string_pretty(config).map_err(|e| anyhow::anyhow!("serialize config: {e}"))?;
        std::fs::write(&self.config_path, toml_str)?;
        Ok(())
    }

    /// Check whether the config file already has onboarding data.
    fn is_already_onboarded(&self) -> bool {
        if self.config_path.exists()
            && let Ok(cfg) = moltis_config::loader::load_config(&self.config_path)
        {
            return cfg.is_onboarded();
        }
        false
    }

    /// Start the wizard. Returns current step info.
    ///
    /// If `force` is true, the wizard starts even when already onboarded,
    /// allowing the user to reconfigure their identity.
    pub fn wizard_start(&self, force: bool) -> Value {
        if !force && self.is_already_onboarded() {
            return json!({
                "onboarded": true,
                "step": "done",
                "prompt": "Already onboarded!",
            });
        }

        let mut ws = WizardState::new();

        // Pre-populate from existing config so the user can keep values.
        if self.config_path.exists()
            && let Ok(cfg) = moltis_config::loader::load_config(&self.config_path)
        {
            ws.identity = cfg.identity;
            ws.user = cfg.user;
        }

        let resp = step_response(&ws);
        *self.state.lock().unwrap() = Some(ws);
        resp
    }

    /// Advance the wizard with user input.
    pub fn wizard_next(&self, input: &str) -> Result<Value, String> {
        let mut guard = self.state.lock().unwrap();
        let ws = guard.as_mut().ok_or("no active wizard session")?;
        ws.advance(input);

        if ws.is_done() {
            // Merge into existing config or create new one.
            let mut config = if self.config_path.exists() {
                moltis_config::loader::load_config(&self.config_path).unwrap_or_default()
            } else {
                MoltisConfig::default()
            };
            config.identity = ws.identity.clone();
            config.user = ws.user.clone();
            self.save(&config)
                .map_err(|e| format!("failed to save config: {e}"))?;

            let resp = json!({
                "step": "done",
                "prompt": ws.prompt(),
                "done": true,
                "identity": {
                    "name": config.identity.name,
                    "emoji": config.identity.emoji,
                    "creature": config.identity.creature,
                    "vibe": config.identity.vibe,
                },
                "user": {
                    "name": config.user.name,
                    "timezone": config.user.timezone,
                },
            });
            *guard = None;
            return Ok(resp);
        }

        Ok(step_response(ws))
    }

    /// Cancel an active wizard session.
    pub fn wizard_cancel(&self) {
        *self.state.lock().unwrap() = None;
    }

    /// Return the current wizard status.
    pub fn wizard_status(&self) -> Value {
        let guard = self.state.lock().unwrap();
        let onboarded = self.is_already_onboarded();
        match guard.as_ref() {
            Some(ws) => json!({
                "active": true,
                "step": ws.step,
                "onboarded": onboarded,
            }),
            None => json!({
                "active": false,
                "onboarded": onboarded,
            }),
        }
    }

    /// Update identity fields by merging partial JSON into the existing config.
    ///
    /// Accepts: `{name?, emoji?, creature?, vibe?, soul?, user_name?}`
    pub fn identity_update(&self, params: Value) -> anyhow::Result<Value> {
        let mut config = if self.config_path.exists() {
            moltis_config::loader::load_config(&self.config_path).unwrap_or_default()
        } else {
            MoltisConfig::default()
        };

        if let Some(v) = params.get("name").and_then(|v| v.as_str()) {
            config.identity.name = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        if let Some(v) = params.get("emoji").and_then(|v| v.as_str()) {
            config.identity.emoji = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        if let Some(v) = params.get("creature").and_then(|v| v.as_str()) {
            config.identity.creature = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        if let Some(v) = params.get("vibe").and_then(|v| v.as_str()) {
            config.identity.vibe = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }
        if let Some(v) = params.get("soul") {
            config.identity.soul = if v.is_null() {
                None
            } else {
                v.as_str().map(|s| s.to_string()).filter(|s| !s.is_empty())
            };
        }
        if let Some(v) = params.get("user_name").and_then(|v| v.as_str()) {
            config.user.name = if v.is_empty() {
                None
            } else {
                Some(v.to_string())
            };
        }

        self.save(&config)?;

        Ok(json!({
            "name": config.identity.name,
            "emoji": config.identity.emoji,
            "creature": config.identity.creature,
            "vibe": config.identity.vibe,
            "soul": config.identity.soul,
            "user_name": config.user.name,
        }))
    }

    /// Update the soul text in the config file.
    pub fn identity_update_soul(&self, soul: Option<String>) -> anyhow::Result<Value> {
        let mut config = if self.config_path.exists() {
            moltis_config::loader::load_config(&self.config_path).unwrap_or_default()
        } else {
            MoltisConfig::default()
        };
        config.identity.soul = soul;
        self.save(&config)?;
        Ok(json!({}))
    }

    /// Read identity from the config file (for `agent.identity.get`).
    pub fn identity_get(&self) -> Value {
        if self.config_path.exists()
            && let Ok(cfg) = moltis_config::loader::load_config(&self.config_path)
        {
            return json!({
                "name": cfg.identity.name.as_deref().unwrap_or("moltis"),
                "emoji": cfg.identity.emoji,
                "creature": cfg.identity.creature,
                "vibe": cfg.identity.vibe,
                "soul": cfg.identity.soul,
                "user_name": cfg.user.name,
            });
        }
        json!({ "name": "moltis", "avatar": null })
    }
}

fn step_response(ws: &WizardState) -> Value {
    json!({
        "step": ws.step,
        "prompt": ws.prompt(),
        "done": ws.step == WizardStep::Done,
        "onboarded": false,
        "current": current_value(ws),
    })
}

/// Returns the current (pre-populated) value for the active step, if any.
fn current_value(ws: &WizardState) -> Option<&str> {
    use WizardStep::*;
    match ws.step {
        UserName => ws.user.name.as_deref(),
        AgentName => ws.identity.name.as_deref(),
        AgentEmoji => ws.identity.emoji.as_deref(),
        AgentCreature => ws.identity.creature.as_deref(),
        AgentVibe => ws.identity.vibe.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, std::io::Write};

    #[test]
    fn wizard_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("moltis.toml");
        let svc = LiveOnboardingService::new(config_path.clone());

        // Start
        let resp = svc.wizard_start(false);
        assert_eq!(resp["onboarded"], false);
        assert_eq!(resp["step"], "welcome");

        // Advance through all steps
        svc.wizard_next("").unwrap(); // welcome → user_name
        svc.wizard_next("Alice").unwrap(); // → agent_name
        svc.wizard_next("Rex").unwrap(); // → emoji
        svc.wizard_next("\u{1f436}").unwrap(); // → creature
        svc.wizard_next("dog").unwrap(); // → vibe
        svc.wizard_next("chill").unwrap(); // → confirm
        let done = svc.wizard_next("").unwrap(); // → done

        assert_eq!(done["done"], true);
        assert_eq!(done["identity"]["name"], "Rex");
        assert_eq!(done["user"]["name"], "Alice");

        // Config file should exist
        assert!(config_path.exists());

        // Should report as onboarded now
        let status = svc.wizard_status();
        assert_eq!(status["onboarded"], true);
    }

    #[test]
    fn already_onboarded() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("moltis.toml");
        // Write a config with identity and user
        let mut f = std::fs::File::create(&config_path).unwrap();
        writeln!(f, "[identity]\nname = \"Rex\"\n\n[user]\nname = \"Alice\"").unwrap();

        let svc = LiveOnboardingService::new(config_path);
        let resp = svc.wizard_start(false);
        assert_eq!(resp["onboarded"], true);
    }

    #[test]
    fn cancel_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let svc = LiveOnboardingService::new(dir.path().join("moltis.toml"));
        svc.wizard_start(false);
        assert_eq!(svc.wizard_status()["active"], true);
        svc.wizard_cancel();
        assert_eq!(svc.wizard_status()["active"], false);
    }

    #[test]
    fn identity_update_partial() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("moltis.toml");
        let svc = LiveOnboardingService::new(config_path.clone());

        // Create initial identity
        let res = svc
            .identity_update(json!({
                "name": "Rex",
                "emoji": "\u{1f436}",
                "creature": "dog",
                "vibe": "chill",
                "user_name": "Alice",
            }))
            .unwrap();
        assert_eq!(res["name"], "Rex");
        assert_eq!(res["user_name"], "Alice");

        // Partial update: only change vibe
        let res = svc.identity_update(json!({ "vibe": "playful" })).unwrap();
        assert_eq!(res["name"], "Rex");
        assert_eq!(res["vibe"], "playful");
        assert_eq!(res["emoji"], "\u{1f436}");

        // Verify identity_get reflects updates
        let id = svc.identity_get();
        assert_eq!(id["name"], "Rex");
        assert_eq!(id["vibe"], "playful");
        assert_eq!(id["user_name"], "Alice");

        // Update soul
        let res = svc
            .identity_update(json!({ "soul": "Be helpful." }))
            .unwrap();
        assert_eq!(res["soul"], "Be helpful.");

        // Clear soul with null
        let res = svc.identity_update(json!({ "soul": null })).unwrap();
        assert!(res["soul"].is_null());

        // Reports as onboarded
        assert_eq!(svc.wizard_status()["onboarded"], true);
    }

    #[test]
    fn identity_update_empty_fields() {
        let dir = tempfile::tempdir().unwrap();
        let svc = LiveOnboardingService::new(dir.path().join("moltis.toml"));

        // Set name, then clear it
        svc.identity_update(json!({ "name": "Rex" })).unwrap();
        let res = svc.identity_update(json!({ "name": "" })).unwrap();
        assert!(res["name"].is_null());
    }
}
