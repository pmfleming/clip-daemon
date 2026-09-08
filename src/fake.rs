use std::{
    collections::HashSet,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    backend::{
        BackendError, BackendMutation, BackendResult, ClipboardBackend, EntryTarget, FileSelection,
        HistoryQuery, ScreenshotRegion,
    },
    classification::{bounded_preview, classify},
    model::{
        BackendStatus, EntryDetails, EntryThumbnail, HistoryPage, OperationResult,
        ReplacementResult,
    },
};

#[derive(Clone)]
pub struct FakeBackend {
    entries: Arc<RwLock<Vec<EntryDetails>>>,
    operation_events: broadcast::Sender<OperationResult>,
}

impl Default for FakeBackend {
    fn default() -> Self {
        let (operation_events, _) = broadcast::channel(16);
        Self {
            entries: Arc::new(RwLock::new(Vec::new())),
            operation_events,
        }
    }
}

impl FakeBackend {
    pub fn with_entries(entries: Vec<EntryDetails>) -> Self {
        Self {
            entries: Arc::new(RwLock::new(entries)),
            ..Self::default()
        }
    }

    fn entries(&self) -> BackendResult<std::sync::RwLockReadGuard<'_, Vec<EntryDetails>>> {
        self.entries.read().map_err(fake_unavailable)
    }

    fn mutate_entry<T>(
        &self,
        opaque_id: &str,
        mutate: impl FnOnce(&mut EntryDetails) -> T,
    ) -> BackendResult<T> {
        let mut entries = self.entries.write().map_err(fake_unavailable)?;
        entries
            .iter_mut()
            .find(|item| item.entry.id == opaque_id)
            .map(mutate)
            .ok_or_else(unknown_entry)
    }

    fn with_entry<T>(
        &self,
        opaque_id: &str,
        read: impl FnOnce(&EntryDetails) -> T,
    ) -> BackendResult<T> {
        self.entries()?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .map(read)
            .ok_or_else(unknown_entry)
    }

    fn validate_revision(&self, opaque_id: &str, expected: Option<u64>) -> BackendResult<()> {
        let actual = self.with_entry(opaque_id, |item| item.entry.revision)?;
        if expected.is_some_and(|revision| revision != actual) {
            return Err(BackendError::stale("Clipboard entry revision is stale"));
        }
        Ok(())
    }

    fn remove(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        let mut entries = self.entries.write().map_err(fake_unavailable)?;
        let position = entries
            .iter()
            .position(|item| item.entry.id == opaque_id)
            .ok_or_else(unknown_entry)?;
        entries.remove(position);
        completed("delete", "Fake entry deleted")
    }

    fn wipe(&self) -> BackendResult<OperationResult> {
        self.entries.write().map_err(fake_unavailable)?.clear();
        completed("wipe", "Fake history cleared")
    }

    fn favorite(&self, opaque_id: &str, favorite: bool) -> BackendResult<OperationResult> {
        self.mutate_entry(opaque_id, |entry| entry.entry.favorite = favorite)?;
        let action = if favorite { "favorite" } else { "unfavorite" };
        completed(action, "Fake favorite updated")
    }
}

#[async_trait]
impl ClipboardBackend for FakeBackend {
    fn operation_events(&self) -> broadcast::Receiver<OperationResult> {
        self.operation_events.subscribe()
    }

    async fn status(&self) -> BackendStatus {
        BackendStatus {
            available: true,
            engine: "fake".into(),
            detail: "test-backend".into(),
        }
    }

    async fn change_token(&self) -> BackendResult<u64> {
        Ok(self.entries()?.iter().map(|item| item.entry.revision).sum())
    }

    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let needle = query.query.to_lowercase();
        let entries = self.entries()?;
        let current = entries
            .iter()
            .find(|item| item.entry.current)
            .map(|item| item.entry.clone());
        let matches: Vec<_> = entries
            .iter()
            .map(|item| &item.entry)
            .filter(|item| needle.is_empty() || item.preview.to_lowercase().contains(&needle))
            .collect();
        let matched = matches.len();
        let page: Vec<_> = matches
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
            .cloned()
            .collect();
        let consumed = query.offset.saturating_add(page.len());
        let has_more = matched > consumed;
        Ok(HistoryPage {
            revision: 1,
            generation: query.generation,
            current,
            entries: page,
            has_more,
            next_offset: has_more.then_some(consumed),
        })
    }

    async fn details(
        &self,
        opaque_id: &str,
        _max_text_bytes: usize,
    ) -> BackendResult<EntryDetails> {
        self.with_entry(opaque_id, Clone::clone)
    }

    async fn revision(&self, opaque_id: &str) -> BackendResult<u64> {
        self.with_entry(opaque_id, |item| item.entry.revision)
    }

    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        _edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        self.validate_revision(opaque_id, expected_revision)?;
        Err(BackendError::not_found(format!(
            "No thumbnail fixture for {}",
            opaque_id
        )))
    }

    async fn capture_screenshot(
        &self,
        _region: ScreenshotRegion,
        _max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        completed("screenshot", "Fake screenshot copied")
    }

    async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        _max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        if mime.is_empty() || bytes.is_empty() {
            return Err(BackendError::new(
                crate::backend::BackendErrorKind::InvalidData,
                "Published clipboard content must have a MIME type and non-empty bytes",
            ));
        }
        completed("publish", "Fake clipboard content published")
    }

    async fn publish_files(
        &self,
        selection: FileSelection,
        _max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        if selection.paths.is_empty() {
            return Err(BackendError::new(
                crate::backend::BackendErrorKind::InvalidData,
                "File selection is empty",
            ));
        }
        completed("publish-files", "Fake file selection published")
    }

    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        if mutation.require_revision(expected_revision)? {
            self.validate_revision(opaque_id, expected_revision)?;
        }
        match mutation {
            BackendMutation::Restore { .. } => completed("copy", "Fake operation completed"),
            BackendMutation::ImageAsFile { .. } => {
                completed("image-as-file", "Fake operation completed")
            }
            BackendMutation::Annotate { .. } => completed("annotate", "Fake operation completed"),
            BackendMutation::Remove => self.remove(opaque_id),
            BackendMutation::SetFavorite(value) => self.favorite(opaque_id, value),
            BackendMutation::Wipe => self.wipe(),
            BackendMutation::Cleanup => completed("cleanup", "Fake caches cleared"),
        }
    }

    async fn remove_many(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        for target in targets {
            self.validate_revision(&target.opaque_id, Some(target.expected_revision))?;
        }
        let ids = targets
            .iter()
            .map(|target| target.opaque_id.as_str())
            .collect::<HashSet<_>>();
        self.entries
            .write()
            .map_err(fake_unavailable)?
            .retain(|entry| !ids.contains(entry.entry.id.as_str()));
        let count = targets.len();
        completed(
            "delete-many",
            &format!("{count} fake clipboard entries deleted"),
        )
    }

    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        self.validate_revision(opaque_id, Some(expected_revision))?;
        let entry = self.mutate_entry(opaque_id, |details| {
            details.entry.revision = details.entry.revision.saturating_add(1);
            details.entry.kind = classify(mime, bytes);
            details.entry.mime = mime.into();
            details.entry.byte_size = bytes.len() as u64;
            details.entry.preview = bounded_preview(bytes, bytes.len());
            details.text = std::str::from_utf8(bytes).ok().map(str::to_owned);
            details.clone()
        })?;
        Ok(ReplacementResult {
            entry,
            selection_published: true,
            publication_message: "Replacement published to the clipboard".into(),
        })
    }

    async fn cancel_operation(&self, _operation_id: &str) -> BackendResult<bool> {
        Ok(false)
    }
}

fn completed(action: &str, message: &str) -> BackendResult<OperationResult> {
    Ok(OperationResult::completed(action, message))
}

fn fake_unavailable<T>(_: std::sync::PoisonError<T>) -> BackendError {
    BackendError::unavailable("Fake clipboard backend is unavailable")
}

fn unknown_entry() -> BackendError {
    BackendError::not_found("Unknown clipboard entry ID")
}
