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
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub revision: u64,
    pub generation: u64,
    pub entries: Vec<EntrySummary>,
    pub has_more: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryDetails {
    pub entry: EntrySummary,
    pub text: Option<String>,
    pub preview_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub available: bool,
    pub engine: String,
    pub detail: String,
}
