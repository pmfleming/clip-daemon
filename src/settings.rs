use std::{
    fs, io,
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
};

use clipboard_history_client_sdk::config;
use clipboard_history_core::dirs::data_dir;
use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{AtomicFilePolicy, StagedFile, XdgRoot, resolve_xdg_path, sync_parent};
use tokio::{
    sync::Mutex as AsyncMutex,
    task::spawn_blocking,
    time::{Duration, sleep},
};

mod services;
use crate::capture::CaptureControl;
use services::{ServiceControl, Systemd, SystemdCapture};

#[derive(Debug, Serialize)]
pub struct CaptureState {
    pub desired_paused: bool,
    pub desired_private_mode: bool,
    pub paused: Option<bool>,
    pub private_mode: bool,
    pub verified: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct RetentionState {
    pub desired_max_entries: u32,
    pub desired_max_favorites: u32,
    pub effective: Option<crate::ringboard::ipc::EngineLimits>,
    pub synchronized: bool,
    pub error: Option<String>,
}

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
    services: Arc<dyn ServiceControl>,
    capture: Arc<dyn CaptureControl>,
}

struct SettingsState {
    value: ClipboardSettings,
    load_error: Option<String>,
    capture_verified: bool,
    capture_error: Option<String>,
}

impl Default for SettingsManager {
    fn default() -> Self {
        let path = settings_path();
        let (value, load_error) = match load_settings(path.as_deref()) {
            Ok(value) => (value, None),
            Err(error) => (ClipboardSettings::default(), Some(error)),
        };
        Self {
            state: StdMutex::new(SettingsState {
                value,
                load_error,
                capture_verified: false,
                capture_error: Some("Capture state has not been verified".into()),
            }),
            path,
            transaction: AsyncMutex::new(()),
            services: Arc::new(Systemd),
            capture: Arc::new(SystemdCapture(Arc::new(Systemd))),
        }
    }
}

type SettingsWriter = fn(Option<&Path>, &ClipboardSettings) -> Result<(), String>;
const SETTINGS_SAVED: &str = "Clipboard settings were saved";
const CAPTURE_SAVED: &str = "Capture preference was saved";

impl SettingsManager {
    pub(crate) fn with_capture(capture: Arc<dyn CaptureControl>) -> Self {
        Self {
            capture,
            ..Self::default()
        }
    }

    /// Legacy booleans never assert privacy unless service state is verified.
    pub fn get(&self) -> Result<ClipboardSettings, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "Clipboard settings are unavailable")?;
        if let Some(error) = &state.load_error {
            return Err(error.clone());
        }
        let mut settings = state.value.clone();
        if !state.capture_verified {
            settings.capture_paused = false;
            settings.private_mode = false;
        }
        Ok(settings)
    }

    fn preferences(&self) -> Result<ClipboardSettings, String> {
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
        let current = self.preferences()?;
        let updated = update.apply(&current)?;
        if updated != current {
            self.save(updated.clone(), persist_config_pair, SETTINGS_SAVED)
                .await?;
        }
        // Compare effective engine state even on a no-op retry after a failed
        // restart. Saved preferences alone are not evidence of applied limits.
        self.apply_retention(&updated)
            .await
            .map_err(|error| format!("{SETTINGS_SAVED}, but {error}"))?;
        self.get()
    }

    /// ExecCondition reads persisted intent, never the unverified legacy view.
    pub fn capture_allowed(&self) -> Result<bool, String> {
        Ok(!self.preferences()?.capture_paused)
    }

    /// Run before starting Ringboard (also used by the packaged service).
    pub fn prepare_engine(&self) -> Result<(), String> {
        persist_config_pair(self.path.as_deref(), &self.preferences()?)
    }

    pub(crate) async fn initialize_retention(&self) -> Result<(), String> {
        let _transaction = self.transaction.lock().await;
        let desired = self.preferences()?;
        self.save(desired.clone(), persist_config_pair, SETTINGS_SAVED)
            .await?;
        self.apply_retention(&desired).await
    }

    pub(crate) async fn retention_state(&self) -> Result<RetentionState, String> {
        let desired = self.preferences()?;
        let result = self.services.limits().await;
        Ok(RetentionState {
            desired_max_entries: desired.max_entries,
            desired_max_favorites: desired.max_favorites,
            synchronized: result
                .as_ref()
                .is_ok_and(|limits| limits_match(limits, &desired)),
            error: result.as_ref().err().cloned(),
            effective: result.ok(),
        })
    }

    async fn apply_retention(&self, desired: &ClipboardSettings) -> Result<(), String> {
        if limits_match(&self.services.limits().await?, desired) {
            return Ok(());
        }
        let _ = self.record_capture(Err("Capture restart is not yet verified".into()));
        let result = async {
            self.capture
                .set_paused(true, desired.max_entry_bytes)
                .await?;
            self.services
                .control("restart", &["ringboard-server.service"])
                .await?;
            sleep(Duration::from_millis(200)).await;
            self.capture
                .set_paused(desired.capture_paused, desired.max_entry_bytes)
                .await?;
            self.verify_capture(desired.capture_paused).await
        }
        .await;
        self.record_capture(result)?;
        if !limits_match(&self.services.limits().await?, desired) {
            return Err("Running Ringboard has not applied the saved retention limits".into());
        }
        Ok(())
    }

    async fn save(
        &self,
        updated: ClipboardSettings,
        write: SettingsWriter,
        saved: &str,
    ) -> Result<ClipboardSettings, String> {
        let path = self.path.clone();
        let updated = spawn_blocking(move || {
            write(path.as_deref(), &updated)?;
            Ok::<_, String>(updated)
        })
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
        if state.value.capture_paused != value.capture_paused
            || state.value.private_mode != value.private_mode
        {
            state.capture_verified = false;
        }
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
        let mut updated = self.preferences()?;
        updated.capture_paused = paused;
        updated.private_mode = paused && private;
        let updated = match self.save(updated, persist, CAPTURE_SAVED).await {
            Ok(updated) => updated,
            Err(error) => {
                // Stop locally even if intent cannot be persisted. Never claim
                // that this pause survives restart or matches saved settings.
                if paused {
                    let _ = self.capture.set_paused(true, 0).await;
                    let _ = self.record_capture(Err(error.clone()));
                }
                return Err(error);
            }
        };
        // Idempotent control is intentional: a prior failure or external restart
        // must never turn a repeated request into a false success.
        self.apply_capture(&updated).await?;
        self.get()
    }

    pub fn capture_state(&self) -> Result<CaptureState, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "Capture state is unavailable")?;
        Ok(CaptureState {
            desired_paused: state.value.capture_paused,
            desired_private_mode: state.value.private_mode,
            paused: state.capture_verified.then_some(state.value.capture_paused),
            private_mode: state.capture_verified && state.value.private_mode,
            verified: state.capture_verified,
            error: state.capture_error.clone(),
        })
    }

    fn record_capture(&self, result: Result<(), String>) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Capture state is unavailable")?;
        state.capture_verified = result.is_ok();
        state.capture_error = result.as_ref().err().cloned();
        result
    }

    async fn verify_capture(&self, desired: bool) -> Result<(), String> {
        let actual = self.capture.is_paused().await?;
        (actual == desired)
            .then_some(())
            .ok_or_else(|| "Capture service does not match the saved preference".into())
    }

    async fn apply_capture(&self, updated: &ClipboardSettings) -> Result<(), String> {
        let _ = self.record_capture(Err("Capture transition is not yet verified".into()));
        let result = async {
            self.capture
                .set_paused(updated.capture_paused, updated.max_entry_bytes)
                .await?;
            self.verify_capture(updated.capture_paused).await
        }
        .await;
        self.record_capture(result)
    }

    pub async fn reconcile_capture(&self) -> Result<(), String> {
        let _transaction = self.transaction.lock().await;
        self.apply_capture(&self.preferences()?).await
    }

    pub async fn refresh_capture(&self) -> Result<(), String> {
        let _transaction = self.transaction.lock().await;
        self.record_capture(
            self.verify_capture(self.preferences()?.capture_paused)
                .await,
        )
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

fn limits_match(limits: &crate::ringboard::ipc::EngineLimits, desired: &ClipboardSettings) -> bool {
    limits.max_entries == desired.max_entries
        && limits.max_favorites == desired.max_favorites
        && limits.max_entry_bytes
            == Some(
                desired
                    .max_entry_bytes
                    .min(crate::backend::MAX_WAYLAND_SELECTION_BYTES),
            )
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
    resolve_xdg_path(XdgRoot::State, "clip-daemon", Path::new("settings.json"))
}

fn load_settings(path: Option<&Path>) -> Result<ClipboardSettings, String> {
    let Some(path) = path else {
        return Ok(ClipboardSettings::default());
    };
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return import_native_settings();
        }
        Err(_) => return Err("Clipboard settings could not be read".into()),
    };
    let value = serde_json::from_slice(&bytes)
        .map_err(|_| "Clipboard settings file is invalid; refusing to use defaults".to_owned())?;
    validate_loaded_settings(value)
}

fn validate_loaded_settings(value: ClipboardSettings) -> Result<ClipboardSettings, String> {
    SettingsUpdate {
        max_entries: Some(value.max_entries),
        max_favorites: Some(value.max_favorites),
        max_entry_bytes: Some(value.max_entry_bytes),
        collapse_self_echoes: None,
    }
    .apply(&value)?;
    if value.private_mode && !value.capture_paused {
        return Err("Private mode requires a paused capture preference".into());
    }
    Ok(value)
}

fn import_native_settings() -> Result<ClipboardSettings, String> {
    let path = data_dir().join(config::server::file_name());
    let mut value = ClipboardSettings::default();
    if path
        .try_exists()
        .map_err(|_| "Native Ringboard configuration is unreadable")?
    {
        let native =
            config::server::load(path).map_err(|_| "Native Ringboard configuration is invalid")?;
        value.max_entries = native.max_entries.main.get();
        value.max_favorites = native.max_entries.favorites.get();
    }
    let limit_path = data_dir().join("clip-daemon-max-bytes");
    if limit_path
        .try_exists()
        .map_err(|_| "Native capture limit is unreadable")?
    {
        let bytes = fs::read(limit_path).map_err(|_| "Native capture limit is unreadable")?;
        if bytes.len() > 32 {
            return Err("Native capture limit is invalid".into());
        }
        value.max_entry_bytes = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|value| value.trim().parse().ok())
            .ok_or("Native capture limit is invalid")?;
    }
    validate_loaded_settings(value)
}

const CLIPBOARD: &str = "Clipboard settings";
const RINGBOARD: &str = "Ringboard settings";

fn persist_config_pair(path: Option<&Path>, value: &ClipboardSettings) -> Result<(), String> {
    let (ringboard_path, ringboard_bytes) = encoded_ringboard_config(value)?;
    let limit_path = data_dir().join("clip-daemon-max-bytes");
    let limit = format!("{}\n", value.max_entry_bytes);
    let mut writes = vec![
        stage_config(&ringboard_path, &ringboard_bytes, RINGBOARD)?,
        stage_config(&limit_path, limit.as_bytes(), "Ringboard capture limit")?,
    ];
    if let Some(path) = path {
        let bytes = serde_json::to_vec_pretty(value)
            .map_err(|_| "Clipboard settings could not be encoded")?;
        writes.push(stage_config(path, &bytes, CLIPBOARD)?);
    }
    commit_all(writes)
}

fn persist(path: Option<&Path>, value: &ClipboardSettings) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "Clipboard settings could not be encoded")?;
    commit_all(vec![stage_config(path, &bytes, CLIPBOARD)?])
}

struct StagedWrite {
    path: PathBuf,
    file: StagedFile,
    previous: Option<Vec<u8>>,
    label: &'static str,
}

fn stage_config(path: &Path, bytes: &[u8], label: &'static str) -> Result<StagedWrite, String> {
    Ok(StagedWrite {
        path: path.to_owned(),
        previous: read_existing(path)?,
        file: StagedFile::new(path, bytes, AtomicFilePolicy::PRIVATE)
            .map_err(|error| format!("{label} could not be staged: {error}"))?,
        label,
    })
}

// Best-effort coordinated updates, not a crash-atomic multi-file transaction.
fn commit_all(mut writes: Vec<StagedWrite>) -> Result<(), String> {
    for write in &mut writes {
        if let Err(error) = write.file.commit() {
            let error = format!("{} could not be committed: {error}", write.label);
            return Err(rollback_writes(&writes, error));
        }
    }
    Ok(())
}

fn rollback_writes(writes: &[StagedWrite], error: String) -> String {
    let failures = writes
        .iter()
        .rev()
        .filter_map(|write| restore_previous(&write.path, write.previous.as_deref()).err())
        .collect::<Vec<_>>();
    if failures.is_empty() {
        error
    } else {
        format!("{error}; rollback failed: {}", failures.join("; "))
    }
}

fn atomic_write(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    shelllist_daemon_core::write_bytes_atomic(path, bytes, AtomicFilePolicy::PRIVATE)
        .map_err(|error| format!("{label} could not be written: {error}"))
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
        Some(bytes) => atomic_write(path, bytes, "Ringboard settings rollback"),
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

    use super::{ClipboardSettings, SettingsManager, SettingsState, SettingsUpdate, load_settings};

    fn manager(path: Option<std::path::PathBuf>) -> SettingsManager {
        SettingsManager {
            state: std::sync::Mutex::new(SettingsState {
                value: ClipboardSettings::default(),
                load_error: None,
                capture_verified: false,
                capture_error: None,
            }),
            path,
            transaction: Default::default(),
            services: std::sync::Arc::new(super::Systemd),
            capture: std::sync::Arc::new(super::SystemdCapture(std::sync::Arc::new(
                super::Systemd,
            ))),
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

    #[derive(Default)]
    struct MockServices {
        attempts: std::sync::atomic::AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
        paused: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl super::ServiceControl for MockServices {
        async fn control(&self, action: &str, _: &[&str]) -> Result<(), String> {
            use std::sync::atomic::Ordering::SeqCst;
            self.attempts.fetch_add(1, SeqCst);
            if self.fail.load(SeqCst) {
                return Err("injected failure".into());
            }
            self.paused.store(action == "stop", SeqCst);
            Ok(())
        }
        async fn capture_paused(&self) -> Result<bool, String> {
            Ok(self.paused.load(std::sync::atomic::Ordering::SeqCst))
        }
    }

    #[tokio::test]
    async fn failed_pause_retries_and_never_asserts_unverified_privacy() {
        use std::sync::{Arc, atomic::Ordering::SeqCst};
        let services = Arc::new(MockServices::default());
        let mut manager = manager(None);
        manager.services = services.clone();
        manager.capture = Arc::new(super::SystemdCapture(services.clone()));
        services.fail.store(true, SeqCst);
        assert!(manager.set_paused(true, true).await.is_err());
        assert!(!manager.get().unwrap().private_mode);
        assert!(manager.capture_state().unwrap().desired_private_mode);
        assert!(!manager.capture_allowed().unwrap());
        assert_eq!(manager.capture_state().unwrap().paused, None);
        services.fail.store(false, SeqCst);
        assert!(manager.set_paused(true, true).await.unwrap().private_mode);
        assert_eq!(services.attempts.load(SeqCst), 2);
        // Detect an external capture restart instead of retaining a privacy claim.
        services.paused.store(false, SeqCst);
        assert!(manager.refresh_capture().await.is_err());
        assert!(!manager.get().unwrap().private_mode);
        manager.reconcile_capture().await.unwrap();
        assert!(manager.get().unwrap().private_mode);
        manager.set_paused(false, false).await.unwrap();
        assert!(manager.capture_allowed().unwrap());
    }

    #[tokio::test]
    async fn failed_persistence_stops_capture_without_claiming_durable_privacy() {
        use std::sync::{Arc, atomic::Ordering::SeqCst};
        let directory = tempdir().unwrap();
        // A directory cannot be replaced by the atomic settings file.
        let mut manager = manager(Some(directory.path().to_owned()));
        let services = Arc::new(MockServices::default());
        manager.capture = Arc::new(super::SystemdCapture(services.clone()));
        assert!(manager.set_paused(true, true).await.is_err());
        assert!(services.paused.load(SeqCst));
        let state = manager.capture_state().unwrap();
        assert!(!state.verified);
        assert!(!state.private_mode);
        assert!(!state.desired_paused);
    }

    #[test]
    fn malformed_settings_are_reported_instead_of_defaulted() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        fs::write(&path, b"{broken").unwrap();
        let error = load_settings(Some(&path)).unwrap_err();
        assert!(error.contains("refusing to use defaults"));
    }
}
