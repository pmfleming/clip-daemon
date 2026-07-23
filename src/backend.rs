use std::{error::Error, fmt};

use async_trait::async_trait;

use crate::model::{BackendStatus, EntryDetails, EntryThumbnail, HistoryPage, OperationResult};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendMutation {
    Restore,
    ImageAsFile,
    Annotate,
    Remove,
    SetFavorite(bool),
    Wipe,
    Cleanup,
}

impl BackendMutation {
    pub fn for_action(action: &str) -> Option<Self> {
        match action {
            "copy" | "paste" => Some(Self::Restore),
            "image-as-file" => Some(Self::ImageAsFile),
            "annotate" => Some(Self::Annotate),
            "delete" => Some(Self::Remove),
            "favorite" | "pin-current" => Some(Self::SetFavorite(true)),
            "unfavorite" => Some(Self::SetFavorite(false)),
            "cleanup" => Some(Self::Cleanup),
            _ => None,
        }
    }
}

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn status(&self) -> BackendStatus;
    async fn change_token(&self) -> BackendResult<u64>;
    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage>;
    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails>;
    async fn thumbnail(&self, opaque_id: &str, edge: u32) -> BackendResult<EntryThumbnail>;
    async fn mutate(
        &self,
        opaque_id: &str,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult>;
    async fn replace(
        &self,
        opaque_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntryDetails>;
    async fn cancel_operation(&self, operation_id: &str) -> BackendResult<bool>;
}
