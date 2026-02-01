//! `boot-md` hook: reads `BOOT.md` from the workspace on `GatewayStart` and
//! feeds it as a user message to the agent.

use std::path::PathBuf;

use {
    anyhow::Result,
    async_trait::async_trait,
    tracing::{debug, info},
};

use moltis_common::hooks::{HookAction, HookEvent, HookHandler, HookPayload};

/// Reads a workspace `BOOT.md` file and injects its content on startup.
pub struct BootMdHook {
    workspace_dir: PathBuf,
}

impl BootMdHook {
    pub fn new(workspace_dir: PathBuf) -> Self {
        Self { workspace_dir }
    }
}

#[async_trait]
impl HookHandler for BootMdHook {
    fn name(&self) -> &str {
        "boot-md"
    }

    fn events(&self) -> &[HookEvent] {
        &[HookEvent::GatewayStart]
    }

    fn priority(&self) -> i32 {
        100 // Run early
    }

    async fn handle(&self, _event: HookEvent, _payload: &HookPayload) -> Result<HookAction> {
        let boot_path = self.workspace_dir.join("BOOT.md");
        if !boot_path.exists() {
            debug!(path = %boot_path.display(), "no BOOT.md found, skipping");
            return Ok(HookAction::Continue);
        }

        let content = tokio::fs::read_to_string(&boot_path).await?;
        if content.trim().is_empty() {
            debug!("BOOT.md is empty, skipping");
            return Ok(HookAction::Continue);
        }

        info!(
            path = %boot_path.display(),
            len = content.len(),
            "loaded BOOT.md for startup injection"
        );

        // Return the content as a ModifyPayload so the gateway can inject it.
        Ok(HookAction::ModifyPayload(serde_json::json!({
            "boot_message": content.trim(),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn boot_md_reads_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("BOOT.md"), "Hello from BOOT.md").unwrap();

        let hook = BootMdHook::new(tmp.path().to_path_buf());
        let payload = HookPayload::GatewayStart {
            address: "127.0.0.1:8080".into(),
        };
        let result = hook
            .handle(HookEvent::GatewayStart, &payload)
            .await
            .unwrap();
        match result {
            HookAction::ModifyPayload(v) => {
                assert_eq!(v["boot_message"], "Hello from BOOT.md");
            },
            _ => panic!("expected ModifyPayload"),
        }
    }

    #[tokio::test]
    async fn boot_md_missing_file_continues() {
        let tmp = tempfile::tempdir().unwrap();
        let hook = BootMdHook::new(tmp.path().to_path_buf());
        let payload = HookPayload::GatewayStart {
            address: "127.0.0.1:8080".into(),
        };
        let result = hook
            .handle(HookEvent::GatewayStart, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }

    #[tokio::test]
    async fn boot_md_empty_file_continues() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("BOOT.md"), "  \n  ").unwrap();

        let hook = BootMdHook::new(tmp.path().to_path_buf());
        let payload = HookPayload::GatewayStart {
            address: "127.0.0.1:8080".into(),
        };
        let result = hook
            .handle(HookEvent::GatewayStart, &payload)
            .await
            .unwrap();
        assert!(matches!(result, HookAction::Continue));
    }
}
