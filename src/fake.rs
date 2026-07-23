use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::{
    backend::{ClipboardBackend, HistoryQuery},
    model::{BackendStatus, EntryDetails, EntrySummary, HistoryPage},
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

    async fn query(&self, query: HistoryQuery) -> Result<HistoryPage> {
        let needle = query.query.to_lowercase();
        let guard = self
            .entries
            .read()
            .map_err(|_| anyhow!("fake backend lock poisoned"))?;
        let matches: Vec<EntrySummary> = guard
            .iter()
            .map(|item| item.entry.clone())
            .filter(|item| needle.is_empty() || item.preview.to_lowercase().contains(&needle))
            .collect();
        let has_more = matches.len() > query.limit;
        Ok(HistoryPage {
            revision: 1,
            generation: query.generation,
            entries: matches.into_iter().take(query.limit).collect(),
            has_more,
        })
    }

    async fn details(&self, opaque_id: &str, _max_text_bytes: usize) -> Result<EntryDetails> {
        self.entries
            .read()
            .map_err(|_| anyhow!("fake backend lock poisoned"))?
            .iter()
            .find(|item| item.entry.id == opaque_id)
            .cloned()
            .ok_or_else(|| anyhow!("unknown clipboard entry ID"))
    }
}
