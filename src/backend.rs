use anyhow::Result;
use async_trait::async_trait;

use crate::model::{BackendStatus, EntryDetails, HistoryPage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub query: String,
    pub generation: u64,
    pub limit: usize,
}

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn status(&self) -> BackendStatus;
    async fn query(&self, query: HistoryQuery) -> Result<HistoryPage>;
    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> Result<EntryDetails>;
}
