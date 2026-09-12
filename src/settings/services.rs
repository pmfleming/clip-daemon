use std::time::Duration;

use async_trait::async_trait;
use tokio::process::Command;

#[async_trait]
pub(super) trait ServiceControl: Send + Sync {
    async fn control(&self, action: &str, units: &[&str]) -> Result<(), String>;
    async fn capture_paused(&self) -> Result<bool, String>;
    async fn limits(&self) -> Result<crate::ringboard::ipc::EngineLimits, String> {
        tokio::task::spawn_blocking(crate::ringboard::ipc::limits)
            .await
            .map_err(|_| "Engine status task failed".to_owned())?
            .map_err(|error| error.to_string())
    }
}

pub(super) struct Systemd;

async fn output(arguments: &[&str]) -> Result<std::process::Output, String> {
    let mut command = Command::new("systemctl");
    command.arg("--user").args(arguments).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .map_err(|_| "Ringboard service control timed out".to_owned())?
        .map_err(|_| "Could not control Ringboard services".to_owned())
}

#[async_trait]
impl ServiceControl for Systemd {
    async fn control(&self, action: &str, units: &[&str]) -> Result<(), String> {
        let args: Vec<_> = std::iter::once(action)
            .chain(units.iter().copied())
            .collect();
        output(&args)
            .await?
            .status
            .success()
            .then_some(())
            .ok_or_else(|| "Ringboard service rejected the request".into())
    }

    async fn capture_paused(&self) -> Result<bool, String> {
        let result = output(&[
            "show",
            "--property=ActiveState",
            "--value",
            "ringboard-wayland.service",
        ])
        .await?;
        if !result.status.success() {
            return Err("Capture service state is unavailable".into());
        }
        match result.stdout.as_slice().trim_ascii() {
            b"inactive" | b"failed" => Ok(true),
            b"active" => Ok(false),
            _ => Err("Capture service is transitioning or unavailable".into()),
        }
    }
}
