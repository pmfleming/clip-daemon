use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use tokio::sync::broadcast;

use crate::{
    backend::{
        BackendError, BackendMutation, BackendResult, ClipboardBackend, FileSelection,
        HistoryQuery, ScreenshotRegion,
    },
    classification::{bounded_preview, classify},
    model::{
        BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage, OperationResult,
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
        self.entries
            .read()
            .map_err(|_| BackendError::unavailable("Fake clipboard backend is unavailable"))
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

    fn operation(&self, opaque_id: &str, action: &str) -> BackendResult<OperationResult> {
        self.entries()?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .ok_or_else(unknown_entry)?;
        completed(action, "Fake operation completed")
    }

    fn validate_revision(&self, opaque_id: &str, expected: Option<u64>) -> BackendResult<()> {
        let actual = self
            .entries()?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .map(|item| item.entry.revision)
            .ok_or_else(unknown_entry)?;
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
        let matches: Vec<EntrySummary> = entries
            .iter()
            .map(|item| item.entry.clone())
            .filter(|item| needle.is_empty() || item.preview.to_lowercase().contains(&needle))
            .collect();
        let matched = matches.len();
        let page: Vec<_> = matches
            .into_iter()
            .skip(query.offset)
            .take(query.limit)
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
        self.entries()?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .cloned()
            .ok_or_else(unknown_entry)
    }

    async fn revision(&self, opaque_id: &str) -> BackendResult<u64> {
        self.entries()?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .map(|item| item.entry.revision)
            .ok_or_else(unknown_entry)
    }

    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        _edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        self.validate_revision(opaque_id, Some(expected_revision))?;
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
            BackendMutation::Restore { .. } => self.operation(opaque_id, "copy"),
            BackendMutation::ImageAsFile { .. } => self.operation(opaque_id, "image-as-file"),
            BackendMutation::Annotate { .. } => self.operation(opaque_id, "annotate"),
            BackendMutation::Remove => self.remove(opaque_id),
            BackendMutation::SetFavorite(value) => self.favorite(opaque_id, value),
            BackendMutation::Wipe => {
                self.entries.write().map_err(fake_unavailable)?.clear();
                completed("wipe", "Fake history cleared")
            }
            BackendMutation::Cleanup => completed("cleanup", "Fake caches cleared"),
        }
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

#[cfg(test)]
mod tests {
    use crate::{
        backend::{BackendMutation, ClipboardBackend, HistoryQuery},
        model::{EntryDetails, EntryKind, EntrySummary},
    };

    use super::FakeBackend;

    fn detail(id: &str, preview: &str, revision: u64) -> EntryDetails {
        EntryDetails {
            entry: EntrySummary {
                id: id.into(),
                revision,
                kind: EntryKind::Text,
                mime: "text/plain".into(),
                byte_size: preview.len() as u64,
                favorite: false,
                current: id == "current",
                preview: preview.into(),
            },
            text: Some(preview.into()),
            files: vec![],
            image: None,
            preview_truncated: false,
        }
    }

    fn query(query: &str, generation: u64, offset: usize, limit: usize) -> HistoryQuery {
        HistoryQuery {
            query: query.into(),
            generation,
            offset,
            limit,
            collapse_self_echoes: true,
        }
    }

    #[tokio::test]
    async fn query_details_and_change_tokens_are_deterministic() {
        let backend = FakeBackend::with_entries(vec![
            detail("current", "alpha", 2),
            detail("other", "beta", 3),
        ]);
        let page = backend.query(query("bet", 7, 0, 10)).await.unwrap();
        assert_eq!(page.generation, 7);
        assert_eq!(page.next_offset, None);
        assert_eq!(page.entries[0].id, "other");
        assert_eq!(page.current.unwrap().id, "current");

        let first = backend.query(query("", 8, 0, 1)).await.unwrap();
        assert_eq!(first.entries[0].id, "current");
        assert_eq!(first.next_offset, Some(1));
        let second = backend.query(query("", 8, 1, 1)).await.unwrap();
        assert_eq!(second.entries[0].id, "other");
        assert_eq!(second.next_offset, None);

        assert_eq!(
            backend.details("other", 10).await.unwrap().text.as_deref(),
            Some("beta")
        );
        assert_eq!(backend.change_token().await.unwrap(), 5);
        assert_eq!(backend.revision("other").await.unwrap(), 3);
        assert!(backend.details("missing", 10).await.is_err());
        let stale_thumbnail = backend.thumbnail("other", 2, 512).await.unwrap_err();
        assert_eq!(stale_thumbnail.kind.code(), "stale-action");
        let stale = backend
            .mutate("other", Some(2), BackendMutation::Remove)
            .await
            .unwrap_err();
        assert_eq!(stale.kind.code(), "stale-action");
        backend
            .mutate("other", Some(3), BackendMutation::SetFavorite(true))
            .await
            .unwrap();
        assert!(backend.details("other", 10).await.unwrap().entry.favorite);
        backend
            .mutate("other", Some(3), BackendMutation::Remove)
            .await
            .unwrap();
        assert!(backend.details("other", 10).await.is_err());
    }
}
