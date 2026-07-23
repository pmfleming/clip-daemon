use std::sync::{Arc, RwLock};

use async_trait::async_trait;

use crate::{
    backend::{BackendError, BackendResult, ClipboardBackend, HistoryQuery},
    model::{BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage},
};

#[derive(Clone, Default)]
pub struct FakeBackend {
    entries: Arc<RwLock<Vec<EntryDetails>>>,
}

impl FakeBackend {
    pub fn with_entries(entries: Vec<EntryDetails>) -> Self {
        Self {
            entries: Arc::new(RwLock::new(entries)),
        }
    }

    fn entries(&self) -> BackendResult<std::sync::RwLockReadGuard<'_, Vec<EntryDetails>>> {
        self.entries
            .read()
            .map_err(|_| BackendError::unavailable("Fake clipboard backend is unavailable"))
    }
}

#[async_trait]
impl ClipboardBackend for FakeBackend {
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
        let has_more = matches.len() > query.limit;
        Ok(HistoryPage {
            revision: 1,
            generation: query.generation,
            current,
            entries: matches.into_iter().take(query.limit).collect(),
            has_more,
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
            .ok_or_else(|| BackendError::not_found("Unknown clipboard entry ID"))
    }

    async fn thumbnail(&self, opaque_id: &str, _edge: u32) -> BackendResult<EntryThumbnail> {
        let details = self.details(opaque_id, 0).await?;
        Err(BackendError::not_found(format!(
            "No thumbnail fixture for {}",
            details.entry.id
        )))
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        backend::{ClipboardBackend, HistoryQuery},
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

    #[tokio::test]
    async fn query_details_and_change_tokens_are_deterministic() {
        let backend = FakeBackend::with_entries(vec![
            detail("current", "alpha", 2),
            detail("other", "beta", 3),
        ]);
        let page = backend
            .query(HistoryQuery {
                query: "bet".into(),
                generation: 7,
                limit: 10,
            })
            .await
            .unwrap();
        assert_eq!(page.generation, 7);
        assert_eq!(page.entries[0].id, "other");
        assert_eq!(page.current.unwrap().id, "current");
        assert_eq!(
            backend.details("other", 10).await.unwrap().text.as_deref(),
            Some("beta")
        );
        assert_eq!(backend.change_token().await.unwrap(), 5);
        assert!(backend.details("missing", 10).await.is_err());
    }
}
