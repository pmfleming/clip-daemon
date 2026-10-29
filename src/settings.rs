use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{AtomicFilePolicy, XdgRoot, resolve_xdg_path};
use tokio::{sync::Mutex as AsyncMutex, task::spawn_blocking};

mod services;
use crate::{
    capture::CaptureControl,
    history_store::{HistoryStore, StorePolicy},
};
use services::PolicyControl;

const MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct RetentionLimits {
    max_entries: u32,
    max_favorites: u32,
    max_entry_bytes: u64,
    max_total_bytes: u64,
}

impl From<StorePolicy> for RetentionLimits {
    fn from(policy: StorePolicy) -> Self {
        Self {
            max_entries: policy.max_entries,
            max_favorites: policy.max_favorites,
            max_entry_bytes: policy.max_entry_bytes,
            max_total_bytes: policy.max_total_bytes,
        }
    }
}

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
    pub desired_max_entry_bytes: u64,
    pub desired_max_total_bytes: u64,
    pub effective: Option<RetentionLimits>,
    pub synchronized: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
            max_entries: 1000,
            max_favorites: 100,
            max_entry_bytes: 16 * 1024 * 1024,
            capture_paused: true,
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
            max_entries: validated_update(self.max_entries, value.max_entries, 1..=100_000)?,
            max_favorites: validated_update(self.max_favorites, value.max_favorites, 1..=10_000)?,
            max_entry_bytes: validated_update(
                self.max_entry_bytes,
                value.max_entry_bytes,
                64 * 1024..=64 * 1024 * 1024,
            )?,
            collapse_self_echoes: self
                .collapse_self_echoes
                .unwrap_or(value.collapse_self_echoes),
            ..*value
        })
    }
}

pub struct SettingsManager {
    state: StdMutex<SettingsState>,
    path: Option<PathBuf>,
    transaction: AsyncMutex<()>,
    stopping: AtomicBool,
    policy: Arc<dyn PolicyControl>,
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
            stopping: AtomicBool::new(false),
            policy: Arc::new(services::Unavailable),
            capture: Arc::new(crate::capture::Unavailable),
        }
    }
}

/// Dropping an incomplete transition leaves admission closed. Never resume from
/// Drop: cancelled/destructive operations must not replay queued clipboard data.
pub(crate) struct CaptureGuard<'a> {
    manager: &'a SettingsManager,
    _transaction: tokio::sync::MutexGuard<'a, ()>,
}

impl CaptureGuard<'_> {
    pub(crate) async fn resume(self) -> Result<(), String> {
        self.manager
            .apply_capture(&self.manager.preferences()?)
            .await
    }
}

type SettingsWriter = fn(Option<&Path>, &ClipboardSettings) -> Result<(), String>;
const SETTINGS_SAVED: &str = "Clipboard settings were saved";
const CAPTURE_SAVED: &str = "Capture preference was saved";

impl SettingsManager {
    #[cfg(test)]
    pub(crate) fn isolated() -> Self {
        Self {
            state: StdMutex::new(SettingsState {
                value: ClipboardSettings::default(),
                load_error: None,
                capture_verified: false,
                capture_error: None,
            }),
            path: None,
            transaction: AsyncMutex::new(()),
            stopping: AtomicBool::new(false),
            policy: Arc::new(services::Unavailable),
            capture: Arc::new(crate::capture::Unavailable),
        }
    }

    pub(crate) fn with_capture(capture: Arc<dyn CaptureControl>) -> Self {
        Self {
            capture,
            ..Self::default()
        }
    }

    pub(crate) fn with_store(capture: Arc<dyn CaptureControl>, store: HistoryStore) -> Self {
        Self {
            capture,
            policy: Arc::new(store),
            ..Self::default()
        }
    }

    /// Legacy booleans never assert privacy unless capture state is verified.
    pub fn get(&self) -> Result<ClipboardSettings, String> {
        let state = self.validated_state()?;
        let mut settings = state.value;
        if !state.capture_verified {
            settings.capture_paused = false;
            settings.private_mode = false;
        }
        Ok(settings)
    }

    fn preferences(&self) -> Result<ClipboardSettings, String> {
        Ok(self.validated_state()?.value)
    }

    fn lock_state(&self, error: &str) -> Result<MutexGuard<'_, SettingsState>, String> {
        self.state.lock().map_err(|_| error.to_owned())
    }

    fn validated_state(&self) -> Result<MutexGuard<'_, SettingsState>, String> {
        if self.stopping.load(Ordering::Acquire) {
            return Err("Clipboard service is shutting down".into());
        }
        let state = self.lock_state("Clipboard settings are unavailable")?;
        if let Some(error) = &state.load_error {
            return Err(error.clone());
        }
        Ok(state)
    }

    pub async fn update(&self, update: SettingsUpdate) -> Result<ClipboardSettings, String> {
        let _transaction = self.transaction.lock().await;
        let current = self.preferences()?;
        let mut updated = update.apply(&current)?;
        if retention_changed(&current, &updated) {
            self.quiesce_locked().await?;
        }
        if updated != current {
            updated = self.save(updated, persist, SETTINGS_SAVED).await?;
        }
        // A saved preference is not evidence of applied SQL policy. Retry even
        // when the requested value is unchanged after a previous failure.
        self.apply_retention(&updated)
            .await
            .map_err(|error| format!("{SETTINGS_SAVED}, but {error}"))?;
        self.apply_capture(&updated)
            .await
            .map_err(|error| format!("{SETTINGS_SAVED}, but {error}"))?;
        self.get()
    }

    pub(crate) async fn initialize_retention(&self) -> Result<(), String> {
        let _transaction = self.transaction.lock().await;
        // Store admission starts closed. Do not pause the untouched controller:
        // a valid active preference must retain its first-start bootstrap.
        self.policy.apply(store_policy(&self.preferences()?)).await
    }

    pub(crate) async fn retention_state(&self) -> Result<RetentionState, String> {
        let desired = self.preferences()?;
        let result = self.policy.policy().await;
        Ok(RetentionState {
            desired_max_entries: desired.max_entries,
            desired_max_favorites: desired.max_favorites,
            desired_max_entry_bytes: desired.max_entry_bytes,
            desired_max_total_bytes: MAX_TOTAL_BYTES,
            synchronized: result
                .as_ref()
                .is_ok_and(|limits| limits_match(limits, &desired)),
            error: result.as_ref().err().cloned(),
            effective: result.ok().map(Into::into),
        })
    }

    async fn retention_matches(&self, desired: &ClipboardSettings) -> bool {
        self.policy
            .policy()
            .await
            .is_ok_and(|limits| limits_match(&limits, desired))
    }

    async fn apply_retention(&self, desired: &ClipboardSettings) -> Result<(), String> {
        if self.retention_matches(desired).await {
            return Ok(());
        }
        self.quiesce_locked().await?;
        self.policy.apply(store_policy(desired)).await
    }

    async fn quiesce_locked(&self) -> Result<(), String> {
        let _ = self.record_capture(Err("Capture is quiesced for a policy transition".into()));
        self.capture.set_paused(true, 0).await?;
        self.close_admission().await
    }

    async fn close_admission(&self) -> Result<(), String> {
        self.policy.close().await
    }

    pub(crate) async fn quiesce(&self) -> Result<CaptureGuard<'_>, String> {
        let transaction = self.transaction.lock().await;
        self.quiesce_locked().await?;
        Ok(CaptureGuard {
            manager: self,
            _transaction: transaction,
        })
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
        let mut state = self.lock_state("Clipboard settings are unavailable")?;
        if state.value.capture_paused != value.capture_paused
            || state.value.private_mode != value.private_mode
        {
            state.capture_verified = false;
        }
        state.value = value;
        state.load_error = None;
        Ok(value)
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
                    let _ = self.close_admission().await;
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
        let state = self.lock_state("Capture state is unavailable")?;
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
        let mut state = self.lock_state("Capture state is unavailable")?;
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
            if !updated.capture_paused && !self.retention_matches(updated).await {
                self.quiesce_locked().await?;
                return Err(
                    "Capture is disabled until store retention/limits are synchronized".into(),
                );
            }
            self.capture
                .set_paused(updated.capture_paused, updated.max_entry_bytes)
                .await?;
            if updated.capture_paused {
                self.close_admission().await?;
            }
            self.verify_capture(updated.capture_paused).await
        }
        .await;
        self.record_capture(result)
    }

    pub(crate) async fn stop_capture(&self) -> Result<(), String> {
        // Serialize with policy/wipe/resume transitions and prevent a request
        // already accepted by a transport from reopening capture at shutdown.
        let _transaction = self.transaction.lock().await;
        self.stopping.store(true, Ordering::Release);
        self.quiesce_locked().await
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
    if update.is_some_and(|value| !range.contains(&value)) {
        return Err("Clipboard setting is outside the supported range".into());
    }
    Ok(update.unwrap_or(current))
}

fn retention_changed(a: &ClipboardSettings, b: &ClipboardSettings) -> bool {
    (a.max_entries, a.max_favorites, a.max_entry_bytes)
        != (b.max_entries, b.max_favorites, b.max_entry_bytes)
}

fn limits_match(limits: &StorePolicy, desired: &ClipboardSettings) -> bool {
    limits.max_entries == desired.max_entries
        && limits.max_favorites == desired.max_favorites
        && limits.max_entry_bytes == desired.max_entry_bytes
        && limits.max_total_bytes == MAX_TOTAL_BYTES
}

fn store_policy(value: &ClipboardSettings) -> StorePolicy {
    StorePolicy {
        admission: false,
        max_entries: value.max_entries,
        max_favorites: value.max_favorites,
        max_entry_bytes: value.max_entry_bytes,
        max_total_bytes: MAX_TOTAL_BYTES,
    }
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
            return Ok(ClipboardSettings::default());
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

fn persist(path: Option<&Path>, value: &ClipboardSettings) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let bytes =
        serde_json::to_vec_pretty(value).map_err(|_| "Clipboard settings could not be encoded")?;
    shelllist_daemon_core::write_bytes_atomic(path, &bytes, AtomicFilePolicy::PRIVATE)
        .map_err(|error| format!("Clipboard settings could not be written: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::{Arc, atomic::Ordering::SeqCst},
    };

    use tempfile::tempdir;

    use super::{ClipboardSettings, SettingsManager, SettingsState, load_settings};

    fn manager(path: Option<std::path::PathBuf>) -> (SettingsManager, Arc<MockServices>) {
        let services = Arc::new(MockServices::default());
        let manager = SettingsManager {
            state: std::sync::Mutex::new(SettingsState {
                value: ClipboardSettings {
                    capture_paused: false,
                    ..Default::default()
                },
                load_error: None,
                capture_verified: false,
                capture_error: None,
            }),
            path,
            transaction: Default::default(),
            stopping: Default::default(),
            policy: services.clone(),
            capture: services.clone(),
        };
        (manager, services)
    }

    #[derive(Default)]
    struct MockServices {
        attempts: std::sync::atomic::AtomicUsize,
        fail: std::sync::atomic::AtomicBool,
        paused: std::sync::atomic::AtomicBool,
        policy: std::sync::Mutex<super::StorePolicy>,
        fail_policy: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl super::PolicyControl for MockServices {
        async fn policy(&self) -> Result<super::StorePolicy, String> {
            Ok(self.policy.lock().unwrap().clone())
        }
        async fn apply(&self, policy: super::StorePolicy) -> Result<(), String> {
            if self.fail_policy.load(SeqCst) {
                return Err("injected policy failure".into());
            }
            *self.policy.lock().unwrap() = policy;
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl crate::capture::CaptureControl for MockServices {
        async fn set_paused(&self, paused: bool, _: u64) -> Result<(), String> {
            self.attempts.fetch_add(1, SeqCst);
            if self.fail.load(SeqCst) {
                return Err("injected failure".into());
            }
            self.paused.store(paused, SeqCst);
            self.policy.lock().unwrap().admission = !paused;
            Ok(())
        }
        async fn is_paused(&self) -> Result<bool, String> {
            Ok(self.paused.load(SeqCst))
        }
    }

    #[tokio::test]
    async fn failed_pause_retries_and_never_asserts_unverified_privacy() {
        let (manager, services) = manager(None);
        services.fail.store(true, SeqCst);
        assert!(manager.set_paused(true, true).await.is_err());
        assert!(!manager.get().unwrap().private_mode);
        assert!(manager.capture_state().unwrap().desired_private_mode);
        assert!(manager.preferences().unwrap().capture_paused);
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
        assert!(!manager.preferences().unwrap().capture_paused);
    }

    #[tokio::test]
    async fn abandoned_quiesce_stays_closed_and_serializes_policy_changes() {
        let (manager, services) = manager(None);
        let guard = manager.quiesce().await.unwrap();
        assert!(services.paused.load(SeqCst));
        assert!(!manager.capture_state().unwrap().verified);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(10),
                manager.set_paused(false, false)
            )
            .await
            .is_err()
        );
        drop(guard);
        assert!(services.paused.load(SeqCst));
        manager.quiesce().await.unwrap().resume().await.unwrap();
        assert!(!services.paused.load(SeqCst));
        assert!(manager.capture_state().unwrap().verified);
    }

    #[tokio::test]
    async fn failed_persistence_stops_capture_without_claiming_durable_privacy() {
        let directory = tempdir().unwrap();
        // A directory cannot be replaced by the atomic settings file.
        let (manager, services) = manager(Some(directory.path().to_owned()));
        assert!(manager.set_paused(true, true).await.is_err());
        assert!(services.paused.load(SeqCst));
        let state = manager.capture_state().unwrap();
        assert!(!state.verified);
        assert!(!state.private_mode);
        assert!(!state.desired_paused);
    }

    #[tokio::test]
    async fn paused_settings_apply_to_the_real_sql_store() {
        let directory = tempdir().unwrap();
        let store = super::HistoryStore::open(&directory.path().join("history.db")).unwrap();
        let (mut manager, _) = manager(None);
        manager.state.lock().unwrap().value.capture_paused = true;
        manager.policy = Arc::new(store.clone());
        manager.initialize_retention().await.unwrap();
        manager
            .update(super::SettingsUpdate {
                max_entries: Some(25),
                max_favorites: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        let effective = store.policy().unwrap();
        assert_eq!(effective.max_entries, 25);
        assert_eq!(effective.max_favorites, 10);
        assert!(!effective.admission);
        assert!(store.insert("text/plain", b"must not be captured").is_err());
        assert!(manager.retention_state().await.unwrap().synchronized);
        store.shutdown().unwrap();
    }

    #[tokio::test]
    async fn shutdown_fences_capture_and_rejects_late_resume() {
        let (manager, services) = manager(None);
        manager.reconcile_capture().await.unwrap();
        assert!(!services.paused.load(SeqCst));
        manager.stop_capture().await.unwrap();
        assert!(services.paused.load(SeqCst));
        assert!(!services.policy.lock().unwrap().admission);
        assert!(manager.set_paused(false, false).await.is_err());
        assert!(manager.reconcile_capture().await.is_err());
        assert!(services.paused.load(SeqCst));
    }

    #[tokio::test]
    async fn startup_policy_is_closed_without_consuming_capture_bootstrap() {
        let (manager, services) = manager(None);
        manager.initialize_retention().await.unwrap();
        assert!(!services.policy.lock().unwrap().admission);
        assert_eq!(services.attempts.load(SeqCst), 0);
        manager.reconcile_capture().await.unwrap();
        assert!(!services.paused.load(SeqCst));
        assert!(services.policy.lock().unwrap().admission);
    }

    #[tokio::test]
    async fn retention_is_applied_directly_and_failure_stays_paused() {
        let (manager, services) = manager(None);
        manager
            .update(super::SettingsUpdate {
                max_entries: Some(42),
                max_entry_bytes: Some(64 * 1024 * 1024),
                ..Default::default()
            })
            .await
            .unwrap();
        let policy = services.policy.lock().unwrap().clone();
        assert_eq!(policy.max_entries, 42);
        assert_eq!(policy.max_entry_bytes, 64 * 1024 * 1024);
        assert_eq!(policy.max_total_bytes, super::MAX_TOTAL_BYTES);
        assert!(manager.retention_state().await.unwrap().synchronized);
        services.fail_policy.store(true, SeqCst);
        assert!(
            manager
                .update(super::SettingsUpdate {
                    max_entries: Some(43),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(services.paused.load(SeqCst));
        assert!(!services.policy.lock().unwrap().admission);
        assert!(!manager.capture_state().unwrap().verified);
        services.fail_policy.store(false, SeqCst);
        manager
            .update(super::SettingsUpdate {
                max_entries: Some(43),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(!services.paused.load(SeqCst));
        assert_eq!(services.policy.lock().unwrap().max_entries, 43);
    }

    #[test]
    fn missing_capture_intent_and_missing_file_default_to_paused() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("settings.json");
        for value in [
            load_settings(None).unwrap(),
            load_settings(Some(&path)).unwrap(),
        ] {
            assert!(value.capture_paused);
            assert_eq!(value.max_entries, 1000);
            assert_eq!(value.max_favorites, 100);
            assert_eq!(value.max_entry_bytes, 16 * 1024 * 1024);
        }
        fs::write(&path, b"{}").unwrap();
        assert!(load_settings(Some(&path)).unwrap().capture_paused);
        fs::write(&path, br#"{"max_entries":2}"#).unwrap();
        assert!(load_settings(Some(&path)).unwrap().capture_paused);
        fs::write(&path, br#"{"capture_paused":false}"#).unwrap();
        assert!(!load_settings(Some(&path)).unwrap().capture_paused);
    }

    #[test]
    fn oversized_and_inconsistent_privacy_preferences_are_rejected() {
        let defaults = ClipboardSettings::default();
        assert!(
            super::SettingsUpdate {
                max_entry_bytes: Some(64 * 1024 * 1024 + 1),
                ..Default::default()
            }
            .apply(&defaults)
            .is_err()
        );
        assert!(
            super::validate_loaded_settings(ClipboardSettings {
                private_mode: true,
                capture_paused: false,
                ..defaults
            })
            .is_err()
        );
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
