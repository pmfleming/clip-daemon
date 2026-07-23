use std::{collections::HashMap, sync::Mutex};

use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader};
use sha2::{Digest, Sha256};

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, ClipboardBackend, HistoryQuery},
    classification::{INSPECTION_LIMIT, bounded_preview, classify},
    model::{
        BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage, OperationResult,
    },
};

mod content;
mod mutation;

use content::{create_thumbnail, detail_facts, invalid_entry, read_bounded};

const DEFAULT_MIME: &str = "text/plain";
const MAX_QUERY_LIMIT: usize = 200;
const MAX_DETAILS_BYTES: usize = 256 * 1024;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 100;

#[derive(Default)]
struct RevisionState {
    token: Option<u64>,
    revision: u64,
}

struct QueryAccumulator<'a> {
    needle: &'a str,
    current_id: Option<u64>,
    limit: usize,
    matched: usize,
    current: Option<EntrySummary>,
    entries: Vec<EntrySummary>,
}

impl QueryAccumulator<'_> {
    fn add(&mut self, raw_id: u64, summary: EntrySummary) {
        if self.current_id == Some(raw_id) {
            self.current = Some(summary.clone());
        }
        if !matches_query(&summary, self.needle) {
            return;
        }
        self.matched += 1;
        if self.entries.len() < self.limit {
            self.entries.push(summary);
        }
    }
}

pub struct RingboardBackend {
    ids: Mutex<HashMap<String, u64>>,
    revision: Mutex<RevisionState>,
}

impl Default for RingboardBackend {
    fn default() -> Self {
        Self {
            ids: Mutex::new(HashMap::new()),
            revision: Mutex::new(RevisionState::default()),
        }
    }
}

impl RingboardBackend {
    fn open() -> BackendResult<(DatabaseReader, EntryReader)> {
        let mut directory = clipboard_history_client_sdk::core::dirs::data_dir();
        let database = DatabaseReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard history is unavailable"))?;
        let reader = EntryReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard entries are unavailable"))?;
        Ok((database, reader))
    }

    fn selected(&self, opaque_id: &str) -> BackendResult<(Entry, EntryReader, EntrySummary)> {
        let raw_id = self.resolve(opaque_id)?;
        let (database, mut reader) = Self::open()?;
        let entry = database
            .get_raw(raw_id)
            .map_err(|_| BackendError::not_found("Clipboard entry is stale or missing"))?;
        let summary = self.summarize(entry, &mut reader, false)?;
        Ok((entry, reader, summary))
    }

    fn summarize(
        &self,
        entry: Entry,
        reader: &mut EntryReader,
        current: bool,
    ) -> BackendResult<EntrySummary> {
        let mut loaded = entry
            .to_file(reader)
            .map_err(|_| invalid_entry("Could not open clipboard entry"))?;
        let byte_size = loaded
            .metadata()
            .map_err(|_| invalid_entry("Could not read clipboard entry metadata"))?
            .len();
        let mime_value = loaded
            .mime_type()
            .map_err(|_| invalid_entry("Could not read clipboard MIME metadata"))?;
        let mime = mime_or_default(mime_value.as_str());
        let bytes = read_bounded(&mut loaded, INSPECTION_LIMIT)?;
        let id = opaque_id(entry.id());
        self.remember(id.clone(), entry.id())?;
        Ok(EntrySummary {
            revision: entry_revision(entry.id(), byte_size, mime),
            id,
            kind: classify(mime, &bytes),
            mime: mime.to_owned(),
            byte_size,
            favorite: entry.ring()
                == clipboard_history_client_sdk::core::protocol::RingKind::Favorites,
            current,
            preview: bounded_preview(&bytes, INSPECTION_LIMIT),
        })
    }

    fn remember(&self, opaque: String, raw: u64) -> BackendResult<()> {
        self.ids
            .lock()
            .map_err(|_| lock_error())?
            .insert(opaque, raw);
        Ok(())
    }

    fn revision_for(&self, token: u64) -> BackendResult<u64> {
        let mut state = self.revision.lock().map_err(|_| lock_error())?;
        if state.token != Some(token) {
            state.token = Some(token);
            state.revision = state.revision.saturating_add(1).max(1);
        }
        Ok(state.revision)
    }

    fn resolve(&self, opaque: &str) -> BackendResult<u64> {
        self.ids
            .lock()
            .map_err(|_| lock_error())?
            .get(opaque)
            .copied()
            .ok_or_else(|| BackendError::not_found("Clipboard entry ID is unknown or stale"))
    }
}

#[async_trait]
impl ClipboardBackend for RingboardBackend {
    async fn status(&self) -> BackendStatus {
        match Self::open() {
            Ok(_) => BackendStatus {
                available: true,
                engine: "ringboard".into(),
                detail: "database-readable".into(),
            },
            Err(error) => BackendStatus {
                available: false,
                engine: "ringboard".into(),
                detail: error.to_string(),
            },
        }
    }

    async fn change_token(&self) -> BackendResult<u64> {
        let (database, _) = Self::open()?;
        Ok(history_token(&database))
    }

    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let (database, mut reader) = Self::open()?;
        let main: Vec<_> = database.main().rev().collect();
        let current_id = main.first().map(Entry::id);
        let needle = query.query.trim().to_lowercase();
        let limit = query.limit.clamp(1, MAX_QUERY_LIMIT);
        let mut results = QueryAccumulator {
            needle: &needle,
            current_id,
            limit,
            matched: 0,
            current: None,
            entries: Vec::with_capacity(limit),
        };
        for entry in database.favorites().rev().chain(main) {
            let summary = self.summarize(entry, &mut reader, current_id == Some(entry.id()))?;
            results.add(entry.id(), summary);
        }
        Ok(HistoryPage {
            revision: self.revision_for(history_token(&database))?,
            generation: query.generation,
            current: results.current,
            has_more: results.matched > results.entries.len(),
            entries: results.entries,
        })
    }

    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let (entry, mut reader, summary) = self.selected(opaque_id)?;
        let mut loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard details"))?;
        let limit = max_text_bytes.min(MAX_DETAILS_BYTES);
        let mut bytes = read_bounded(&mut loaded, limit.saturating_add(1))?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
        let (files, image) = detail_facts(&summary, &bytes);
        Ok(EntryDetails {
            entry: summary,
            text,
            files,
            image,
            preview_truncated: truncated,
        })
    }

    async fn thumbnail(&self, opaque_id: &str, edge: u32) -> BackendResult<EntryThumbnail> {
        let (entry, mut reader, summary) = self.selected(opaque_id)?;
        let loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard image"))?;
        create_thumbnail(&loaded, &summary, edge)
    }

    async fn restore(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        self.restore_entry(opaque_id)
    }

    async fn image_as_file(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        self.save_image_file(opaque_id)
    }

    async fn annotate(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        self.start_annotation(opaque_id)
    }

    async fn wipe(&self) -> BackendResult<OperationResult> {
        self.wipe_entries()
    }
}

fn mime_or_default(mime: &str) -> &str {
    if mime.is_empty() { DEFAULT_MIME } else { mime }
}

fn history_token(database: &DatabaseReader) -> u64 {
    let main = database.main();
    let favorites = database.favorites();
    u64::from(main.ring().write_head())
        | (u64::from(favorites.ring().write_head()) << 32)
            ^ (main.ring().len() as u64).rotate_left(17)
            ^ (favorites.ring().len() as u64).rotate_left(49)
}

fn matches_query(summary: &EntrySummary, needle: &str) -> bool {
    needle.is_empty()
        || summary.preview.to_lowercase().contains(needle)
        || summary.mime.to_lowercase().contains(needle)
}

fn lock_error() -> BackendError {
    BackendError::new(
        BackendErrorKind::OperationFailed,
        "Clipboard backend state is unavailable",
    )
}

fn opaque_id(raw_id: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:ringboard:v1:");
    hasher.update(raw_id.to_le_bytes());
    let digest = hasher.finalize();
    format!("entry-{}", hex::encode(&digest[..16]))
}

fn entry_revision(raw_id: u64, size: u64, mime: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:revision:v1:");
    hasher.update(raw_id.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mime.as_bytes());
    u64::from_le_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("fixed SHA-256 prefix"),
    )
}

#[cfg(test)]
mod tests {
    use super::{entry_revision, opaque_id};

    #[test]
    fn engine_ids_are_not_exposed_and_revisions_are_stable() {
        assert!(opaque_id(42).starts_with("entry-"));
        assert!(!opaque_id(42).contains("42"));
        assert_eq!(opaque_id(42), opaque_id(42));
        assert_eq!(
            entry_revision(1, 2, "text/plain"),
            entry_revision(1, 2, "text/plain")
        );
        assert_ne!(
            entry_revision(1, 2, "text/plain"),
            entry_revision(1, 3, "text/plain")
        );
    }
}
