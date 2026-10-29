//! Clipboard actions backed exclusively by the bounded relational history worker.
use crate::{
    backend::{
        BackendError, BackendErrorKind, BackendMutation, BackendResult, ClipboardBackend,
        EntryTarget, FileSelection, HistoryQuery, ScreenshotRegion, invalid_data,
        operation_error as operation_failed,
    },
    history_store::{HistoryStore, StoredEntry},
    model::{
        BackendStatus, EntryDetails, EntryThumbnail, HistoryPage, OperationResult,
        ReplacementResult,
    },
    selection::SelectionService,
};
use async_trait::async_trait;
use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{sync::broadcast, task::spawn_blocking};
use url::Url;
mod artifacts;
mod capture;
pub(crate) mod content;
mod mutation;
mod operation;
mod screenshot;
use artifacts::ArtifactRegistry;
use operation::Operations;
const MAX_DETAILS_BYTES: usize = 256 * 1024;

macro_rules! run_backend {
    ($source:expr, $method:ident($($argument:expr),* $(,)?)) => {{
        let backend = $source.clone();
        run_blocking(move || {
            let _transaction = backend.transaction.lock().map_err(|_| lock_error())?;
            backend.ensure_running()?;
            backend.$method($($argument),*)
        }).await
    }};
}

#[derive(Clone, Default)]
pub struct HistoryBackend {
    transaction: Arc<Mutex<()>>,
    operation_gate: Arc<tokio::sync::Mutex<()>>,
    closed: Arc<AtomicBool>,
    storage: Arc<Mutex<Option<HistoryStore>>>,
    operations: Arc<Operations>,
    active_artifact: Arc<Mutex<Option<PathBuf>>>,
    selection: SelectionService,
}

impl HistoryBackend {
    pub fn open() -> BackendResult<Self> {
        Ok(Self::from_store(HistoryStore::open_default()?))
    }

    fn ensure_running(&self) -> BackendResult<()> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(BackendError::unavailable("Clipboard backend is shut down"));
        }
        Ok(())
    }

    pub async fn shutdown(&self) -> BackendResult<()> {
        let _gate = self.operation_gate.lock().await;
        self.closed.store(true, Ordering::SeqCst);
        self.operations.stop().await?;
        let backend = self.clone();
        run_blocking(move || {
            let _transaction = backend.transaction.lock().map_err(|_| lock_error())?;
            // Worker shutdown closes submissions, drains accepted requests,
            // persists closed admission and checkpoints. It is idempotent.
            backend.store()?.shutdown()
        })
        .await
    }

    pub fn from_store(store: HistoryStore) -> Self {
        Self {
            storage: Arc::new(Mutex::new(Some(store))),
            ..Self::default()
        }
    }

    /// Lazy initialization keeps Default fallible rather than panicking at startup.
    pub fn store(&self) -> BackendResult<HistoryStore> {
        let mut storage = self.storage.lock().map_err(|_| lock_error())?;
        if storage.is_none() {
            *storage = Some(HistoryStore::open_default()?);
        }
        storage.clone().ok_or_else(lock_error)
    }

    fn artifact_registry(&self) -> BackendResult<ArtifactRegistry> {
        Ok(ArtifactRegistry::new(
            self.store()?,
            self.active_artifact.clone(),
        ))
    }

    fn selected(&self, id: &str, expected: Option<u64>) -> BackendResult<StoredEntry> {
        self.store()?.get(id, expected)
    }

    fn change_token_sync(&self) -> BackendResult<u64> {
        self.store()?.revision()
    }

    fn query_sync(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let page = self.store()?.query(query)?;
        // SQL filters and pages before returning metadata; there is no full-history cache.
        if let Err(error) = self.artifact_registry()?.reconcile() {
            tracing::warn!(%error, "Generated-file reconciliation failed");
        }
        Ok(page)
    }

    fn details_sync(&self, id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        content::entry_details(self.selected(id, None)?, max_text_bytes)
    }

    fn revision_sync(&self, id: &str) -> BackendResult<u64> {
        self.store()?.entry_revision(id)
    }

    fn mutate_sync(
        &self,
        id: &str,
        expected: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        mutation.require_revision(expected)?;
        match mutation {
            BackendMutation::Restore { max_bytes } => self.restore_entry(id, expected, max_bytes),
            BackendMutation::ImageAsFile { max_bytes } => {
                self.save_image_file(id, expected, max_bytes)
            }
            BackendMutation::Remove => self.remove_entry(id, expected),
            BackendMutation::SetFavorite(value) => self.move_entry(id, expected, value),
            BackendMutation::Wipe => self.wipe_entries(),
            BackendMutation::Cleanup => self.cleanup_artifacts(),
            BackendMutation::Annotate { .. } => Err(BackendError::new(
                BackendErrorKind::OperationFailed,
                "External editing must be started asynchronously",
            )),
        }
    }

    fn replace_sync(
        &self,
        id: &str,
        expected: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        let summary = self.store()?.replace(id, expected, mime, bytes)?;
        let details = content::entry_details(
            StoredEntry {
                summary: summary.clone(),
                mime: mime.to_owned(),
                bytes: bytes.to_vec(),
            },
            MAX_DETAILS_BYTES,
        )?;
        let publication = self.restore_entry(
            &summary.id,
            Some(summary.revision),
            MAX_DETAILS_BYTES as u64,
        );
        let (selection_published, publication_message) = match publication {
            Ok(_) => (true, "Replacement published to the clipboard".to_owned()),
            Err(error) => (
                false,
                format!("Replacement committed, but clipboard publication failed: {error}"),
            ),
        };
        Ok(ReplacementResult {
            entry: details,
            selection_published,
            publication_message,
        })
    }

    fn publish_sync(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        if bytes.is_empty() {
            return Err(invalid_data(
                "Published clipboard content must not be empty",
            ));
        }
        self.selection.publish(mime, bytes, max_bytes)?;
        self.selection_changed()?;
        Ok(OperationResult::completed(
            "publish",
            "Content published to the Wayland clipboard",
        ))
    }

    fn publish_sensitive_sync(&self, text: String, seconds: u16) -> BackendResult<OperationResult> {
        // Never insert, stage to disk, or fall back to an ordinary publisher.
        self.selection
            .publish_sensitive(text.into_bytes(), seconds)?;
        Ok(OperationResult::completed(
            "publish-sensitive",
            "Sensitive content copied without history",
        ))
    }

    fn publish_files_sync(
        &self,
        selection: FileSelection,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        selection.validate().map_err(invalid_data)?;
        let count = selection.paths.len();
        self.selection.publish_files(
            selection.operation.as_str(),
            encode_file_uris(&selection.paths)?,
            max_bytes,
        )?;
        self.selection_changed()?;
        Ok(OperationResult::completed(
            "publish-files",
            &format!(
                "Mirrored {count} {} to the clipboard",
                if count == 1 { "file" } else { "files" }
            ),
        ))
    }
}

#[async_trait]
impl ClipboardBackend for HistoryBackend {
    fn active_operations(&self) -> BackendResult<HashSet<String>> {
        self.operations.active_ids()
    }
    fn operation_events(&self) -> broadcast::Receiver<OperationResult> {
        self.operations.subscribe()
    }
    fn operation_status(&self, id: &str) -> BackendResult<Option<OperationResult>> {
        self.operations.status(id)
    }
    async fn status(&self) -> BackendStatus {
        let backend = self.clone();
        let result = run_blocking(move || backend.store()?.revision()).await;
        BackendStatus {
            available: result.is_ok(),
            engine: "turso".into(),
            detail: result.map_or_else(|error| error.to_string(), |_| "database-readable".into()),
        }
    }
    async fn change_token(&self) -> BackendResult<u64> {
        run_backend!(self, change_token_sync())
    }
    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        run_backend!(self, query_sync(query))
    }
    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let id = opaque_id.to_owned();
        run_backend!(self, details_sync(&id, max_text_bytes))
    }
    async fn revision(&self, opaque_id: &str) -> BackendResult<u64> {
        let id = opaque_id.to_owned();
        run_backend!(self, revision_sync(&id))
    }
    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let id = opaque_id.to_owned();
        run_backend!(self, thumbnail_sync(&id, expected_revision, edge))
    }
    async fn capture_screenshot(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        // Hold the launch gate through publication so wipe cannot overtake a direct capture.
        let _gate = self.operation_gate.lock().await;
        self.ensure_running()?;
        self.capture_region(region, max_bytes).await
    }
    async fn interactive_screenshot(
        &self,
        request: crate::backend::InteractiveScreenshot,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let _gate = self.operation_gate.lock().await;
        self.ensure_running()?;
        screenshot::launch(self, request, max_bytes)
    }
    async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let mime = mime.to_owned();
        run_backend!(self, publish_sync(&mime, bytes, max_bytes))
    }
    async fn publish_sensitive(
        &self,
        text: String,
        seconds: u16,
    ) -> BackendResult<OperationResult> {
        run_backend!(self, publish_sensitive_sync(text, seconds))
    }
    async fn publish_files(
        &self,
        selection: FileSelection,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        run_backend!(self, publish_files_sync(selection, max_bytes))
    }
    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        mutation.require_revision(expected_revision)?;
        let _gate = self.operation_gate.lock().await;
        if matches!(mutation, BackendMutation::Wipe | BackendMutation::Cleanup) {
            self.operations.stop().await?;
        }
        let id = opaque_id.to_owned();
        if let BackendMutation::Annotate { max_bytes } = mutation {
            let staged = run_backend!(self, stage_annotation(&id, expected_revision, max_bytes))?;
            return self.launch_annotation(staged);
        }
        run_backend!(self, mutate_sync(&id, expected_revision, mutation))
    }
    async fn remove_many(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        let targets = targets.to_vec();
        run_backend!(self, remove_entries(&targets))
    }
    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        let id = opaque_id.to_owned();
        let mime = mime.to_owned();
        let bytes = bytes.to_vec();
        run_backend!(self, replace_sync(&id, expected_revision, &mime, &bytes))
    }
    async fn cancel_operation(&self, operation_id: &str) -> BackendResult<bool> {
        self.operations.cancel(operation_id).await
    }
}

async fn run_blocking<T: Send + 'static>(
    work: impl FnOnce() -> BackendResult<T> + Send + 'static,
) -> BackendResult<T> {
    spawn_blocking(work)
        .await
        .map_err(|_| operation_failed("Clipboard backend task failed"))?
}
fn encode_file_uris(paths: &[PathBuf]) -> BackendResult<Vec<u8>> {
    paths.iter().try_fold(Vec::new(), |mut bytes, path| {
        let uri = Url::from_file_path(path)
            .map_err(|_| invalid_data("File selection path is invalid"))?;
        bytes.extend_from_slice(uri.as_str().as_bytes());
        bytes.extend_from_slice(b"\r\n");
        Ok(bytes)
    })
}
fn lock_error() -> BackendError {
    operation_failed("Clipboard backend state is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, HistoryBackend) {
        let directory = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&directory.path().join("history.db")).unwrap();
        let mut policy = store.policy().unwrap();
        policy.admission = true;
        store.apply_policy(policy).unwrap();
        (directory, HistoryBackend::from_store(store))
    }
    #[test]
    fn default_does_not_open_or_panic() {
        let backend = HistoryBackend::default();
        assert!(backend.storage.lock().unwrap().is_none());
    }
    #[tokio::test]
    async fn edits_report_committed_content_when_publication_fails() {
        let (_directory, backend) = fixture();
        let store = backend.store().unwrap();
        let entry = store.insert("text/plain", b"before").unwrap();
        // This commits within storage policy but exceeds the publication limit;
        // no Wayland compositor or external process is needed to inject failure.
        let bytes = vec![b'x'; MAX_DETAILS_BYTES + 1];
        let result = backend
            .replace(&entry.id, entry.revision, "text/plain", &bytes)
            .await
            .unwrap();
        assert!(!result.selection_published);
        assert!(result.publication_message.contains("committed"));
        assert_eq!(result.entry.entry.id, entry.id);
        assert!(result.entry.preview_truncated);
        assert_eq!(store.get(&entry.id, None).unwrap().bytes, bytes);
        assert!(
            backend
                .replace(&entry.id, entry.revision, "text/plain", b"stale")
                .await
                .is_err()
        );
        backend.shutdown().await.unwrap();
        backend.shutdown().await.unwrap();
        assert!(
            backend
                .capture_screenshot(
                    ScreenshotRegion {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1
                    },
                    1024
                )
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn details_and_actions_do_not_require_prior_query_or_projection() {
        let (_directory, backend) = fixture();
        let entry = backend
            .store()
            .unwrap()
            .insert("text/plain", b"hello")
            .unwrap();
        assert_eq!(
            backend
                .details(&entry.id, 100)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("hello")
        );
        backend
            .mutate(
                &entry.id,
                Some(entry.revision),
                BackendMutation::SetFavorite(true),
            )
            .await
            .unwrap();
        assert!(
            backend
                .mutate(&entry.id, Some(entry.revision), BackendMutation::Remove)
                .await
                .is_err()
        );
        let revision = backend.revision(&entry.id).await.unwrap();
        backend
            .mutate(&entry.id, Some(revision), BackendMutation::Remove)
            .await
            .unwrap();
        assert!(backend.details(&entry.id, 100).await.is_err());
        backend.shutdown().await.unwrap();
    }
}
