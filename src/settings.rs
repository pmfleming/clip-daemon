use std::{
    env, fs, fs::OpenOptions, io::Write, num::NonZeroU32, os::unix::fs::OpenOptionsExt,
    path::PathBuf, sync::Mutex,
};

use clipboard_history_client_sdk::{config, core::dirs::data_dir};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub max_entries: u32,
    pub max_favorites: u32,
    pub max_entry_bytes: u64,
    pub max_editable_text_bytes: usize,
    pub capture_paused: bool,
    pub private_mode: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            max_entries: 750,
            max_favorites: 100,
            max_entry_bytes: 16 * 1024 * 1024,
            max_editable_text_bytes: 256 * 1024,
            capture_paused: false,
            private_mode: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SettingsUpdate {
    pub max_entries: Option<u32>,
    pub max_favorites: Option<u32>,
    pub max_entry_bytes: Option<u64>,
}

pub struct SettingsManager {
    value: Mutex<ClipboardSettings>,
    path: Option<PathBuf>,
}

impl Default for SettingsManager {
    fn default() -> Self {
        let path = settings_path();
        let value = path
            .as_ref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            value: Mutex::new(value),
            path,
        }
    }
}

impl SettingsManager {
    pub fn get(&self) -> Result<ClipboardSettings, String> {
        self.value
            .lock()
            .map(|value| value.clone())
            .map_err(|_| "Clipboard settings are unavailable".into())
    }

    pub async fn update(&self, update: SettingsUpdate) -> Result<ClipboardSettings, String> {
        let updated = self.apply_update(update)?;
        restart_capture().await?;
        Ok(updated)
    }

    fn apply_update(&self, update: SettingsUpdate) -> Result<ClipboardSettings, String> {
        let mut value = self
            .value
            .lock()
            .map_err(|_| "Clipboard settings are unavailable")?;
        apply_limit(
            "max_entries",
            update.max_entries,
            1..=131_070,
            &mut value.max_entries,
        )?;
        apply_limit(
            "max_favorites",
            update.max_favorites,
            1..=1_022,
            &mut value.max_favorites,
        )?;
        if let Some(max) = update.max_entry_bytes {
            if !(64 * 1024..=512 * 1024 * 1024).contains(&max) {
                return Err("max_entry_bytes must be between 64 KiB and 512 MiB".into());
            }
            value.max_entry_bytes = max;
        }
        persist(self.path.as_ref(), &value)?;
        write_ringboard_config(&value)?;
        Ok(value.clone())
    }

    pub async fn set_paused(
        &self,
        paused: bool,
        private: bool,
    ) -> Result<ClipboardSettings, String> {
        let action = if paused { "stop" } else { "start" };
        let status = Command::new("systemctl")
            .args(["--user", action, "ringboard-wayland.service"])
            .status()
            .await
            .map_err(|_| "Could not control Ringboard capture")?;
        if !status.success() {
            return Err("Ringboard capture service rejected the request".into());
        }
        let mut value = self
            .value
            .lock()
            .map_err(|_| "Clipboard settings are unavailable")?;
        value.capture_paused = paused;
        value.private_mode = paused && private;
        persist(self.path.as_ref(), &value)?;
        Ok(value.clone())
    }
}

fn apply_limit(
    name: &str,
    update: Option<u32>,
    range: std::ops::RangeInclusive<u32>,
    value: &mut u32,
) -> Result<(), String> {
    let Some(update) = update else {
        return Ok(());
    };
    if !range.contains(&update) {
        return Err(format!("{name} is outside Ringboard limits"));
    }
    *value = update;
    Ok(())
}

async fn restart_capture() -> Result<(), String> {
    let status = Command::new("systemctl")
        .args([
            "--user",
            "restart",
            "ringboard-server.service",
            "ringboard-wayland.service",
        ])
        .status()
        .await
        .map_err(|_| "Could not restart Ringboard")?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "Ringboard rejected the retention update".into())
}

fn write_ringboard_config(value: &ClipboardSettings) -> Result<(), String> {
    let config = config::server::Config {
        max_entries: config::server::MaxEntries {
            main: NonZeroU32::new(value.max_entries).ok_or("max_entries cannot be zero")?,
            favorites: NonZeroU32::new(value.max_favorites)
                .ok_or("max_favorites cannot be zero")?,
        },
    };
    let encoded = toml::to_string_pretty(&config::server::Stable::from(config))
        .map_err(|_| "Ringboard settings could not be encoded")?;
    let path = data_dir().join(config::server::file_name());
    fs::create_dir_all(path.parent().ok_or("Ringboard data path is invalid")?)
        .map_err(|_| "Ringboard data directory is unavailable")?;
    fs::write(path, encoded).map_err(|_| "Ringboard settings could not be written".into())
}

fn settings_path() -> Option<PathBuf> {
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(root.join("clip-daemon/settings.json"))
}

fn persist(path: Option<&PathBuf>, value: &ClipboardSettings) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let parent = path.parent().ok_or("Clipboard state path is invalid")?;
    fs::create_dir_all(parent).map_err(|_| "Clipboard state directory is unavailable")?;
    let mut permissions = fs::metadata(parent)
        .map_err(|_| "Clipboard state directory is unavailable")?
        .permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    fs::set_permissions(parent, permissions)
        .map_err(|_| "Clipboard state permissions could not be set")?;
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "Clipboard settings could not be encoded")?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "Clipboard settings could not be written")?;
    file.write_all(&bytes)
        .map_err(|_| "Clipboard settings could not be written".into())
}

#[cfg(test)]
mod tests {
    use super::{SettingsManager, SettingsUpdate};

    #[tokio::test]
    async fn invalid_limits_are_rejected() {
        let manager = SettingsManager {
            value: Default::default(),
            path: None,
        };
        assert!(
            manager
                .update(SettingsUpdate {
                    max_entries: Some(0),
                    ..Default::default()
                })
                .await
                .is_err()
        );
    }
}
