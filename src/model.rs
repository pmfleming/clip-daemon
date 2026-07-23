use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EntryKind {
    Text,
    Link,
    Image,
    Files,
    Html,
    Json,
    Color,
    Binary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntrySummary {
    pub id: String,
    pub revision: u64,
    pub kind: EntryKind,
    pub mime: String,
    pub byte_size: u64,
    pub favorite: bool,
    pub current: bool,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub revision: u64,
    pub generation: u64,
    pub current: Option<EntrySummary>,
    pub entries: Vec<EntrySummary>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageMetadata {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilePreview {
    pub display_name: String,
    pub uri: String,
    pub exists: bool,
    pub operation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryDetails {
    pub entry: EntrySummary,
    pub text: Option<String>,
    pub files: Vec<FilePreview>,
    pub image: Option<ImageMetadata>,
    pub preview_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryThumbnail {
    pub entry_id: String,
    pub revision: u64,
    pub path: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub available: bool,
    pub engine: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationResult {
    pub id: String,
    pub action: String,
    pub status: String,
    pub message: String,
    pub path: Option<String>,
}

impl OperationResult {
    pub fn completed(action: &str, message: &str) -> Self {
        Self {
            id: format!("operation-{}", uuid::Uuid::new_v4()),
            action: action.into(),
            status: "completed".into(),
            message: message.into(),
            path: None,
        }
    }
}
