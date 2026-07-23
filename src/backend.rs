use std::{error::Error, fmt};

use async_trait::async_trait;

use crate::model::{BackendStatus, EntryDetails, EntryThumbnail, HistoryPage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub query: String,
    pub generation: u64,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    Unavailable,
    NotFound,
    InvalidData,
    OperationFailed,
}

impl BackendErrorKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "backend-unavailable",
            Self::NotFound => "entry-not-found",
            Self::InvalidData => "invalid-entry",
            Self::OperationFailed => "operation-failed",
        }
    }

    pub const fn retryable(self) -> bool {
        matches!(self, Self::Unavailable)
    }
}

#[derive(Debug)]
pub struct BackendError {
    pub kind: BackendErrorKind,
    message: String,
}

impl BackendError {
    pub fn new(kind: BackendErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::Unavailable, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::NotFound, message)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for BackendError {}

pub type BackendResult<T> = Result<T, BackendError>;

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn status(&self) -> BackendStatus;
    async fn change_token(&self) -> BackendResult<u64>;
    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage>;
    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails>;
    async fn thumbnail(&self, opaque_id: &str, edge: u32) -> BackendResult<EntryThumbnail>;
}
