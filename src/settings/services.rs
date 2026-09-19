use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

/// Engine lifecycle only. Capture is controlled and verified in-process.
#[async_trait]
pub(super) trait ServiceControl: Send + Sync {
    async fn control(&self, action: &str, units: &[&str]) -> Result<(), String>;
    async fn limits(&self) -> Result<crate::ringboard::ipc::EngineLimits, String> {
        tokio::task::spawn_blocking(crate::ringboard::ipc::limits)
            .await
            .map_err(|_| "Engine status task failed".to_owned())?
            .map_err(|error| error.to_string())
    }
}

pub(super) struct Systemd;

#[async_trait]
impl ServiceControl for Systemd {
    async fn control(&self, action: &str, units: &[&str]) -> Result<(), String> {
        let mut command = Command::new("systemctl");
        command
            .arg("--user")
            .arg(action)
            .args(units)
            .kill_on_drop(true);
        tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .map_err(|_| "Ringboard service control timed out")?
            .map_err(|_| "Could not control Ringboard server")?
            .status
            .success()
            .then_some(())
            .ok_or_else(|| "Ringboard service rejected the request".into())
    }
}
