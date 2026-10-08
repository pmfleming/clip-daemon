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

/// Shared history request; storage and native search use the same paging policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryQuery {
    pub query: String,
    pub generation: u64,
    pub offset: usize,
    pub limit: usize,
    pub collapse_self_echoes: bool,
}

/// Retain only the page, but exhaust the input for totals and per-row effects.
/// The caller supplies its limit policy; zero limits and overflowing offsets
/// must still visit every row. Items can be borrowed, with no `Clone` bound.
pub(crate) fn paginate<T>(
    mut matches: impl Iterator<Item = T>,
    offset: usize,
    limit: usize,
) -> (Vec<T>, usize) {
    let mut matched = 0;
    let page = matches
        .by_ref()
        .inspect(|_| matched += 1)
        .skip(offset)
        .take(limit)
        .collect();
    matched += matches.count();
    (page, matched)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryPage {
    pub revision: u64,
    pub generation: u64,
    pub current: Option<EntrySummary>,
    pub entries: Vec<EntrySummary>,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
pub struct ReplacementResult {
    pub entry: EntryDetails,
    pub selection_published: bool,
    pub publication_message: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl OperationResult {
    pub fn completed(action: &str, message: &str) -> Self {
        Self::with_id(
            format!("operation-{}", uuid::Uuid::new_v4()),
            action,
            "completed",
            message,
        )
    }

    pub fn with_id(id: String, action: &str, status: &str, message: &str) -> Self {
        Self {
            id,
            action: action.into(),
            status: status.into(),
            message: message.into(),
            path: None,
            warning: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::paginate;

    #[test]
    fn paging_visits_every_row_once_even_with_empty_or_overflowing_pages() {
        for length in [0, 5] {
            for offset in [0, 2, 5, usize::MAX] {
                for limit in [0, 1, 3, usize::MAX] {
                    let mut visited = Vec::new();
                    let (page, total) =
                        paginate((0..length).inspect(|row| visited.push(*row)), offset, limit);
                    assert_eq!(
                        page,
                        (0..length).skip(offset).take(limit).collect::<Vec<_>>()
                    );
                    assert_eq!(total, length);
                    assert_eq!(visited, (0..length).collect::<Vec<_>>());
                }
            }
        }
    }
}
