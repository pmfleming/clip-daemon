use std::{
    env, fs,
    fs::{File, OpenOptions},
    io,
    io::Write,
    num::NonZeroU32,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Mutex as StdMutex,
};

use clipboard_history_client_sdk::{config, core::dirs::data_dir};
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    sync::Mutex as AsyncMutex,
    task::spawn_blocking,
    time::{Duration, sleep},
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardSettings {
    pub max_entries: u32,
    pub max_favorites: u32,
    pub max_entry_bytes: u64,
    pub capture_paused: bool,
    pub private_mode: bool,
    pub collapse_self_echoes: bool,
}

impl Default for ClipboardSettings {
    fn default() -> Self {
        Self {
            max_entries: 750,
            max_favorites: 100,
            max_entry_bytes: 16 * 1024 * 1024,
            capture_paused: false,
            private_mode: false,
            collapse_self_echoes: true,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SettingsUpdate {
    pub max_entries: Option<u32>,
    pub max_favorites: Option<u32>,
    pub max_entry_bytes: Option<u64>,
    pub collapse_self_echoes: Option<bool>,
}

impl SettingsUpdate {
    fn apply(self, value: &ClipboardSettings) -> Result<ClipboardSettings, String> {
        Ok(ClipboardSettings {
            max_entries: validated_update(self.max_entries, value.max_entries, 1..=131_070)?,
            max_favorites: validated_update(self.max_favorites, value.max_favorites, 1..=1_022)?,
            max_entry_bytes: validated_update(
                self.max_entry_bytes,
                value.max_entry_bytes,
                64 * 1024..=512 * 1024 * 1024,
            )?,
            capture_paused: value.capture_paused,
            private_mode: value.private_mode,
            collapse_self_echoes: self
                .collapse_self_echoes
                .unwrap_or(value.collapse_self_echoes),
        })
    }
}

pub struct SettingsManager {
    state: StdMutex<SettingsState>,
    path: Option<PathBuf>,
    transaction: AsyncMutex<()>,
}

struct SettingsState {
    value: ClipboardSettings,
    load_error: Option<String>,
}

impl Default for SettingsManager {
    fn default() -> Self {
        let path = settings_path();
        let (value, load_error) = match load_settings(path.as_deref()) {
            Ok(value) => (value, None),
            Err(error) => (ClipboardSettings::default(), Some(error)),
        };
        Self {
            state: StdMutex::new(SettingsState { value, load_error }),
            path,
            transaction: AsyncMutex::new(()),
        }
    }
}

type SettingsWriter = fn(Option<&Path>, &ClipboardSettings) -> Result<(), String>;
const SETTINGS_SAVED: &str = "Clipboard settings were saved";
const CAPTURE_SAVED: &str = "Capture preference was saved";

impl SettingsManager {
    pub fn get(&self) -> Result<ClipboardSettings, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "Clipboard settings are unavailable")?;
        match &state.load_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.value.clone()),
        }
    }

    pub async fn update(&self, update: SettingsUpdate) -> Result<ClipboardSettings, String> {
        let _transaction = self.transaction.lock().await;
        let current = self.get()?;
        let updated = update.apply(&current)?;
        if updated == current {
            return Ok(current);
        }
        let restart_required = updated.max_entries != current.max_entries
            || updated.max_favorites != current.max_favorites
            || updated.max_entry_bytes != current.max_entry_bytes;
        let updated = self
            .save(updated, persist_config_pair, SETTINGS_SAVED)
            .await?;
        if restart_required && let Err(error) = restart_capture(&updated).await {
            return Err(format!("{SETTINGS_SAVED}, but {error}"));
        }
        Ok(updated)
    }

    async fn save(
        &self,
        updated: ClipboardSettings,
        write: SettingsWriter,
        saved: &str,
    ) -> Result<ClipboardSettings, String> {
        let path = self.path.clone();
        let persisted = updated.clone();
        spawn_blocking(move || write(path.as_deref(), &persisted))
            .await
            .map_err(|_| "Clipboard settings transaction failed")??;
        self.commit(updated)
            .map_err(|error| format!("{saved}, but {error}"))
    }

    fn commit(&self, value: ClipboardSettings) -> Result<ClipboardSettings, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Clipboard settings are unavailable")?;
        state.value = value;
        state.load_error = None;
        Ok(state.value.clone())
    }

    pub async fn set_paused(
        &self,
        paused: bool,
        private: bool,
    ) -> Result<ClipboardSettings, String> {
        let _transaction = self.transaction.lock().await;
        let mut updated = self.get()?;
        let private_mode = paused && private;
        if (updated.capture_paused, updated.private_mode) == (paused, private_mode) {
            return Ok(updated);
        }
        updated.capture_paused = paused;
        updated.private_mode = private_mode;
        let updated = self.save(updated, persist, CAPTURE_SAVED).await?;
        let action = if paused { "stop" } else { "start" };
        if let Err(error) = control_units(action, &["ringboard-wayland.service"]).await {
            return Err(format!("{CAPTURE_SAVED}, but {error}"));
        }
        Ok(updated)
    }
}

fn validated_update<T: Copy + PartialOrd>(
    update: Option<T>,
    current: T,
    range: std::ops::RangeInclusive<T>,
) -> Result<T, String> {
    match update {
        Some(value) if range.contains(&value) => Ok(value),
        Some(_) => Err("Clipboard setting is outside the supported range".into()),
        None => Ok(current),
    }
}

async fn restart_capture(settings: &ClipboardSettings) -> Result<(), String> {
    control_units("restart", &["ringboard-server.service"]).await?;
    sleep(Duration::from_millis(200)).await;
    let capture_action = if settings.capture_paused {
        "stop"
    } else {
        "restart"
    };
    control_units(capture_action, &["ringboard-wayland.service"]).await
}

async fn control_units(action: &str, units: &[&str]) -> Result<(), String> {
    let status = Command::new("systemctl")
        .args(["--user", action])
        .args(units)
        .status()
        .await
        .map_err(|_| "Could not control Ringboard services")?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "Ringboard service rejected the request".into())
}

fn encoded_ringboard_config(value: &ClipboardSettings) -> Result<(PathBuf, Vec<u8>), String> {
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
    Ok((path, encoded.into_bytes()))
}

fn settings_path() -> Option<PathBuf> {
    let root = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))?;
    Some(root.join("clip-daemon/settings.json"))
}

fn load_settings(path: Option<&Path>) -> Result<ClipboardSettings, String> {
    let Some(path) = path else {
        return Ok(ClipboardSettings::default());
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ClipboardSettings::default());
        }
        Err(_) => return Err("Clipboard settings could not be read".into()),
    };
    serde_json::from_slice(&bytes)
        .map_err(|_| "Clipboard settings file is invalid; refusing to use defaults".into())
}

const CLIPBOARD: &str = "Clipboard settings";
const RINGBOARD: &str = "Ringboard settings";

fn persist_config_pair(path: Option<&Path>, value: &ClipboardSettings) -> Result<(), String> {
    let (ringboard_path, ringboard_bytes) = encoded_ringboard_config(value)?;
    let mut writes = vec![stage_config(
        &ringboard_path,
        &ringboard_bytes,
        false,
        RINGBOARD,
    )?];
    if let Some(path) = path {
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|_| "Clipboard settings could not be encoded")?;
        writes.push(stage_config(path, &bytes, true, CLIPBOARD)?);
    }
    commit_all(writes)
}

fn persist(path: Option<&Path>, value: &ClipboardSettings) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "Clipboard settings could not be encoded")?;
    commit_all(vec![stage_config(path, &bytes, true, CLIPBOARD)?])
}

struct StagedWrite {
    path: PathBuf,
    temp: PathBuf,
    previous: Option<Vec<u8>>,
    label: &'static str,
}

fn stage_config(
    path: &Path,
    bytes: &[u8],
    private_parent: bool,
    label: &'static str,
) -> Result<StagedWrite, String> {
    Ok(StagedWrite {
        path: path.to_owned(),
        previous: read_existing(path)?,
        temp: stage_write(path, bytes, private_parent, label)?,
        label,
    })
}

fn commit_all(writes: Vec<StagedWrite>) -> Result<(), String> {
    for write in &writes {
        if let Err(error) = commit_staged(&write.path, &write.temp, write.label) {
            let failures: Vec<_> = writes
                .iter()
                .rev()
                .filter_map(|write| restore_previous(&write.path, write.previous.as_deref()).err())
                .collect();
            return if failures.is_empty() {
                Err(error)
            } else {
                Err(format!("{error}; rollback failed: {}", failures.join("; ")))
            };
        }
    }
    Ok(())
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp);
    }
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    private_parent: bool,
    label: &str,
) -> Result<(), String> {
    let temp = stage_write(path, bytes, private_parent, label)?;
    commit_staged(path, &temp, label)
}

fn stage_write(
    path: &Path,
    bytes: &[u8],
    private_parent: bool,
    label: &str,
) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{label} path is invalid"))?;
    fs::create_dir_all(parent).map_err(|_| format!("{label} directory is unavailable"))?;
    if private_parent {
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| format!("{label} directory permissions could not be set"))?;
    }
    let temp = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temp)
        .map_err(|_| format!("{label} temporary file could not be created"))?;
    if file.write_all(bytes).is_err() || file.sync_all().is_err() {
        let _ = fs::remove_file(&temp);
        return Err(format!("{label} could not be written"));
    }
    Ok(temp)
}

fn commit_staged(path: &Path, temp: &Path, label: &str) -> Result<(), String> {
    if fs::rename(temp, path).is_err() {
        let _ = fs::remove_file(temp);
        return Err(format!("{label} could not be committed"));
    }
    sync_parent(path).map_err(|_| format!("{label} directory could not be synced"))
}

fn sync_parent(path: &Path) -> io::Result<()> {
    File::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?.sync_all()
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("Existing settings could not be read".into()),
    }
}

fn restore_previous(path: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(bytes) => atomic_write(path, bytes, false, "Ringboard settings rollback"),
        None => {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(_) => return Err("New settings file could not be removed".to_owned()),
            }
            sync_parent(path).map_err(|_| "Settings directory could not be synced".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        ClipboardSettings, SettingsManager, SettingsState, SettingsUpdate, atomic_write,
        load_settings,
    };

    fn manager(path: Option<std::path::PathBuf>) -> SettingsManager {
        SettingsManager {
            state: std::sync::Mutex::new(SettingsState {
                value: ClipboardSettings::default(),
                load_error: None,
            }),
            path,
            transaction: Default::default(),
        }
    }

    #[tokio::test]
    async fn invalid_limits_are_rejected() {
        let update = SettingsUpdate {
            max_entries: Some(0),
            ..Default::default()
        };
        assert!(manager(None).update(update).await.is_err());
    }

    #[tokio::test]
    async fn no_op_update_does_not_write_or_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let manager = manager(Some(path.clone()));
        assert_eq!(
            manager.update(SettingsUpdate::default()).await.unwrap(),
            ClipboardSettings::default()
        );
        assert!(!path.exists());
    }

    #[test]
    fn malformed_settings_are_reported_instead_of_defaulted() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, b"{broken").unwrap();
        let error = load_settings(Some(&path)).unwrap_err();
        assert!(error.contains("refusing to use defaults"));
    }

    #[test]
    fn atomic_write_replaces_the_complete_file() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state/settings.json");
        atomic_write(&path, b"old", true, "test settings").unwrap();
        atomic_write(&path, b"new-value", true, "test settings").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new-value");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }
}
