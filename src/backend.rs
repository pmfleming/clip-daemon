use std::{error::Error, fmt};

use async_trait::async_trait;

use crate::model::{BackendStatus, EntryDetails, EntryThumbnail, HistoryPage, OperationResult};

pub const MAX_QUERY_LIMIT: usize = 200;
pub const MAX_WAYLAND_SELECTION_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub query: String,
    pub generation: u64,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenshotRegion {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendErrorKind {
    Unavailable,
    NotFound,
    Stale,
    InvalidData,
    OperationFailed,
}

impl BackendErrorKind {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "backend-unavailable",
            Self::NotFound => "entry-not-found",
            Self::Stale => "stale-action",
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

    pub fn stale(message: impl Into<String>) -> Self {
        Self::new(BackendErrorKind::Stale, message)
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
    Restore { max_bytes: u64 },
    ImageAsFile { max_bytes: u64 },
    Annotate { max_bytes: u64 },
    Remove,
    SetFavorite(bool),
    Wipe,
    Cleanup,
}

impl BackendMutation {
    pub fn for_action(action: &str, max_bytes: u64) -> Option<Self> {
        match action {
            "copy" | "paste" => Some(Self::Restore { max_bytes }),
            "image-as-file" => Some(Self::ImageAsFile { max_bytes }),
            "annotate" => Some(Self::Annotate { max_bytes }),
            "delete" => Some(Self::Remove),
            "favorite" | "pin-current" => Some(Self::SetFavorite(true)),
            "unfavorite" => Some(Self::SetFavorite(false)),
            "cleanup" => Some(Self::Cleanup),
            _ => None,
        }
    }

    pub fn require_revision(self, revision: Option<u64>) -> BackendResult<bool> {
        let targets_entry = !matches!(self, Self::Wipe | Self::Cleanup);
        if targets_entry && revision.is_none() {
            return Err(BackendError::stale(
                "An expected clipboard entry revision is required",
            ));
        }
        Ok(targets_entry)
    }
}

#[async_trait]
pub trait ClipboardBackend: Send + Sync {
    async fn status(&self) -> BackendStatus;
    async fn change_token(&self) -> BackendResult<u64>;
    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage>;
    async fn revision(&self, opaque_id: &str) -> BackendResult<u64>;
    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails>;
    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        edge: u32,
    ) -> BackendResult<EntryThumbnail>;
    async fn capture_screenshot(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult>;
    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult>;
    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntryDetails>;
    async fn cancel_operation(&self, operation_id: &str) -> BackendResult<bool>;
}
