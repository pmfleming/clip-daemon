use std::{
    collections::HashMap,
    fs::File,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::task::{JoinError, JoinHandle, spawn_blocking};

use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader, LoadedEntry};
use sha2::{Digest, Sha256};

use crate::{
    backend::{
        BackendError, BackendErrorKind, BackendMutation, BackendResult, ClipboardBackend,
        HistoryQuery, MAX_QUERY_LIMIT, ScreenshotRegion,
    },
    classification::{INSPECTION_LIMIT, bounded_preview},
    model::{
        BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage, OperationResult,
    },
    selection::SelectionService,
};

mod content;
mod mutation;

use content::{
    create_file_thumbnail, create_thumbnail, detail_facts, detected_image_mime, invalid_entry,
    is_inline_image_mime, prune_thumbnails, read_bounded, semantic_kind,
};

const DEFAULT_MIME: &str = "text/plain";
const MAX_DETAILS_BYTES: usize = 256 * 1024;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 100;

macro_rules! run_backend {
    ($source:expr, $method:ident($($argument:expr),* $(,)?)) => {{
        let backend = $source.clone();
        run_blocking(move || backend.$method($($argument),*)).await
    }};
}

#[derive(Default)]
struct RevisionState {
    token: Option<u64>,
    revision: u64,
}

#[derive(Default)]
struct SummaryCache {
    token: Option<u64>,
    entries: HashMap<u64, Option<EntrySummary>>,
}

#[derive(Clone, Copy)]
struct IdentityBinding {
    raw_id: u64,
    revision: u64,
}

impl SummaryCache {
    fn select_token(&mut self, token: u64) {
        if self.token != Some(token) {
            self.token = Some(token);
            self.entries.clear();
        }
    }
}

struct OperationTask {
    handle: JoinHandle<()>,
    files: [PathBuf; 2],
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

#[derive(Clone)]
pub struct RingboardBackend {
    ids: Arc<Mutex<HashMap<String, IdentityBinding>>>,
    revision: Arc<Mutex<RevisionState>>,
    summaries: Arc<Mutex<SummaryCache>>,
    operations: Arc<Mutex<HashMap<String, OperationTask>>>,
    selection: SelectionService,
}

impl Default for RingboardBackend {
    fn default() -> Self {
        Self {
            ids: Arc::new(Mutex::new(HashMap::new())),
            revision: Arc::new(Mutex::new(RevisionState::default())),
            summaries: Arc::new(Mutex::new(SummaryCache::default())),
            operations: Arc::new(Mutex::new(HashMap::new())),
            selection: SelectionService::default(),
        }
    }
}

impl RingboardBackend {
    fn open_database() -> BackendResult<DatabaseReader> {
        let mut directory = clipboard_history_client_sdk::core::dirs::data_dir();
        DatabaseReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard history is unavailable"))
    }

    fn open() -> BackendResult<(DatabaseReader, EntryReader)> {
        let database = Self::open_database()?;
        let mut directory = clipboard_history_client_sdk::core::dirs::data_dir();
        let reader = EntryReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard entries are unavailable"))?;
        Ok((database, reader))
    }

    fn status_sync(&self) -> BackendStatus {
        match Self::open_database() {
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

    fn change_token_sync(&self) -> BackendResult<u64> {
        Ok(history_token(&Self::open_database()?))
    }

    fn selected(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<(Entry, EntryReader, EntrySummary)> {
        let binding = self.resolve(opaque_id)?;
        let (database, mut reader) = Self::open()?;
        let entry = database
            .get_raw(binding.raw_id)
            .map_err(|_| BackendError::stale("Clipboard entry is stale or missing"))?;
        let summary = self.summarize(entry, &mut reader, false)?;
        self.verify_selection(opaque_id, expected_revision, binding, &summary)?;
        Ok((entry, reader, summary))
    }

    fn verify_selection(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        binding: IdentityBinding,
        summary: &EntrySummary,
    ) -> BackendResult<()> {
        let error = if summary.id != opaque_id {
            Some("Clipboard entry ID is stale or has been reused")
        } else if summary.revision != binding.revision
            || expected_revision.is_some_and(|revision| revision != summary.revision)
        {
            Some("Clipboard entry revision changed before the operation")
        } else {
            None
        };
        let Some(message) = error else {
            return Ok(());
        };
        self.ids.lock().map_err(|_| lock_error())?.remove(opaque_id);
        Err(BackendError::stale(message))
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
        let metadata = loaded
            .metadata()
            .map_err(|_| invalid_entry("Could not read clipboard entry metadata"))?;
        let byte_size = metadata.len();
        let bytes = read_bounded(&mut loaded, INSPECTION_LIMIT)?;
        let detected_mime = detected_image_mime(&bytes);
        let mime_value = detected_mime
            .is_none()
            .then(|| stored_mime_type(&loaded))
            .transpose()?;
        let mime = detected_mime
            .or_else(|| {
                mime_value
                    .as_ref()
                    .map(|value| mime_or_default(value.as_str()))
            })
            .unwrap_or(DEFAULT_MIME);
        let fingerprint = entry_fingerprint(entry.id(), byte_size, mime, &bytes);
        let id = opaque_id(&fingerprint);
        let kind = semantic_kind(mime, &bytes);
        Ok(EntrySummary {
            revision: entry_revision(&fingerprint),
            id,
            kind,
            mime: mime.to_owned(),
            byte_size,
            favorite: entry.ring()
                == clipboard_history_client_sdk::core::protocol::RingKind::Favorites,
            current,
            preview: bounded_preview(&bytes, INSPECTION_LIMIT),
        })
    }

    fn cached_summary(
        &self,
        cache: &mut SummaryCache,
        entry: Entry,
        reader: &mut EntryReader,
        current: bool,
    ) -> BackendResult<Option<EntrySummary>> {
        if let Some(summary) = cache.entries.get(&entry.id()) {
            return Ok(summary.clone());
        }
        let summary = match catch_unwind(AssertUnwindSafe(|| {
            self.summarize(entry, reader, current)
        })) {
            Ok(Ok(summary)) => Some(summary),
            Ok(Err(error))
                if matches!(
                    error.kind,
                    BackendErrorKind::InvalidData | BackendErrorKind::NotFound
                ) =>
            {
                tracing::warn!(code = %error.kind.code(), "Skipping unreadable clipboard entry");
                None
            }
            Ok(Err(error)) => return Err(error),
            Err(_) => {
                tracing::warn!("Skipping clipboard entry that panicked while being read");
                None
            }
        };
        cache.entries.insert(entry.id(), summary.clone());
        Ok(summary)
    }

    fn query_sync(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let (database, mut reader) = Self::open()?;
        let token = history_token(&database);
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
        let mut current_ids = HashMap::new();
        let mut valid_thumbnails = Vec::new();
        let mut cache = self.summaries.lock().map_err(|_| lock_error())?;
        cache.select_token(token);
        for entry in database.favorites().rev().chain(main) {
            let raw_id = entry.id();
            if let Some(summary) =
                self.cached_summary(&mut cache, entry, &mut reader, current_id == Some(raw_id))?
            {
                current_ids.insert(
                    summary.id.clone(),
                    IdentityBinding {
                        raw_id,
                        revision: summary.revision,
                    },
                );
                valid_thumbnails.push((summary.id.clone(), summary.revision));
                results.add(raw_id, summary);
            }
        }
        drop(cache);
        *self.ids.lock().map_err(|_| lock_error())? = current_ids;
        prune_thumbnails(&valid_thumbnails);
        Ok(HistoryPage {
            revision: self.revision_for(token)?,
            generation: query.generation,
            current: results.current,
            has_more: results.matched > results.entries.len(),
            entries: results.entries,
        })
    }

    fn details_sync(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let (entry, reader, summary) = self.selected(opaque_id, None)?;
        entry_details(entry, reader, summary, max_text_bytes)
    }

    fn revision_sync(&self, opaque_id: &str) -> BackendResult<u64> {
        self.selected(opaque_id, None)
            .map(|(_, _, summary)| summary.revision)
    }

    fn details_raw_sync(&self, raw_id: u64, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let (database, mut reader) = Self::open()?;
        let entry = database
            .get_raw(raw_id)
            .map_err(|_| BackendError::not_found("Clipboard entry is stale or missing"))?;
        let summary = self.summarize(entry, &mut reader, false)?;
        self.ids.lock().map_err(|_| lock_error())?.insert(
            summary.id.clone(),
            IdentityBinding {
                raw_id,
                revision: summary.revision,
            },
        );
        entry_details(entry, reader, summary, max_text_bytes)
    }

    fn thumbnail_sync(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let (entry, mut reader, summary) = self.selected(opaque_id, Some(expected_revision))?;
        let mut loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard image"))?;
        if summary.kind == crate::model::EntryKind::Image && !is_inline_image_mime(&summary.mime) {
            create_file_thumbnail(&mut loaded, &summary, edge)
        } else if summary.kind == crate::model::EntryKind::Image {
            create_thumbnail(&loaded, &summary, edge)
        } else {
            Err(invalid_entry("Clipboard entry cannot be thumbnailed"))
        }
    }

    fn mutate_sync(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        mutation.require_revision(expected_revision)?;
        match mutation {
            BackendMutation::Restore { max_bytes } => {
                self.restore_entry(opaque_id, expected_revision, max_bytes)
            }
            BackendMutation::ImageAsFile { max_bytes } => {
                self.save_image_file(opaque_id, expected_revision, max_bytes)
            }
            BackendMutation::Remove => self.remove_entry(opaque_id, expected_revision),
            BackendMutation::SetFavorite(value) => {
                self.move_entry(opaque_id, expected_revision, value)
            }
            BackendMutation::Wipe => self.wipe_entries(),
            BackendMutation::Cleanup => self.cleanup_artifacts(),
            BackendMutation::Annotate { .. } => Err(BackendError::new(
                BackendErrorKind::OperationFailed,
                "Annotation must be started asynchronously",
            )),
        }
    }

    fn replace_sync(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntryDetails> {
        let raw_id = self.replace_entry(opaque_id, Some(expected_revision), mime, bytes)?;
        let details = self.details_raw_sync(raw_id, MAX_DETAILS_BYTES)?;
        self.restore_entry(
            &details.entry.id,
            Some(details.entry.revision),
            MAX_DETAILS_BYTES as u64,
        )?;
        Ok(details)
    }

    fn revision_for(&self, token: u64) -> BackendResult<u64> {
        let mut state = self.revision.lock().map_err(|_| lock_error())?;
        if state.token != Some(token) {
            state.token = Some(token);
            state.revision = state.revision.saturating_add(1).max(1);
        }
        Ok(state.revision)
    }

    fn clear_identity_state(&self) -> BackendResult<()> {
        self.ids.lock().map_err(|_| lock_error())?.clear();
        let mut cache = self.summaries.lock().map_err(|_| lock_error())?;
        cache.token = None;
        cache.entries.clear();
        Ok(())
    }

    fn resolve(&self, opaque: &str) -> BackendResult<IdentityBinding> {
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
        let backend = self.clone();
        spawn_blocking(move || backend.status_sync())
            .await
            .unwrap_or_else(|_| BackendStatus {
                available: false,
                engine: "ringboard".into(),
                detail: "Ringboard status task failed".into(),
            })
    }

    async fn change_token(&self) -> BackendResult<u64> {
        run_backend!(self, change_token_sync())
    }

    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        run_backend!(self, query_sync(query))
    }

    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let opaque_id = opaque_id.to_owned();
        run_backend!(self, details_sync(&opaque_id, max_text_bytes))
    }

    async fn revision(&self, opaque_id: &str) -> BackendResult<u64> {
        let opaque_id = opaque_id.to_owned();
        run_backend!(self, revision_sync(&opaque_id))
    }

    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let opaque_id = opaque_id.to_owned();
        run_backend!(self, thumbnail_sync(&opaque_id, expected_revision, edge))
    }

    async fn capture_screenshot(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        run_backend!(self, capture_region(region, max_bytes))
    }

    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        mutation.require_revision(expected_revision)?;
        if let BackendMutation::Annotate { max_bytes } = mutation {
            let opaque_id = opaque_id.to_owned();
            let staged = run_backend!(
                self,
                stage_annotation(&opaque_id, expected_revision, max_bytes)
            )?;
            return self.launch_annotation(staged);
        }
        let opaque_id = opaque_id.to_owned();
        run_backend!(self, mutate_sync(&opaque_id, expected_revision, mutation))
    }

    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntryDetails> {
        let opaque_id = opaque_id.to_owned();
        let mime = mime.to_owned();
        let bytes = bytes.to_vec();
        run_backend!(
            self,
            replace_sync(&opaque_id, expected_revision, &mime, &bytes)
        )
    }

    async fn cancel_operation(&self, operation_id: &str) -> BackendResult<bool> {
        let operation = self
            .operations
            .lock()
            .map_err(|_| lock_error())?
            .remove(operation_id);
        let Some(operation) = operation else {
            return Ok(false);
        };
        let was_running = !operation.handle.is_finished();
        operation.handle.abort();
        let _ = operation.handle.await;
        run_blocking(move || {
            mutation::remove_files(&operation.files);
            Ok(())
        })
        .await?;
        Ok(was_running)
    }
}

fn entry_details(
    entry: Entry,
    mut reader: EntryReader,
    summary: EntrySummary,
    max_text_bytes: usize,
) -> BackendResult<EntryDetails> {
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

async fn run_blocking<T>(
    work: impl FnOnce() -> BackendResult<T> + Send + 'static,
) -> BackendResult<T>
where
    T: Send + 'static,
{
    spawn_blocking(work).await.map_err(blocking_task_error)?
}

fn blocking_task_error(_: JoinError) -> BackendError {
    BackendError::new(
        BackendErrorKind::OperationFailed,
        "Clipboard backend task failed",
    )
}

fn mime_or_default(mime: &str) -> &str {
    if mime.is_empty() { DEFAULT_MIME } else { mime }
}

fn stored_mime_type(loaded: &LoadedEntry<'_, File>) -> BackendResult<String> {
    let Some(file) = loaded.backing_file() else {
        return loaded
            .mime_type()
            .map(|mime| mime.as_str().to_owned())
            .map_err(|_| invalid_entry("Could not read clipboard MIME metadata"));
    };
    let mut bytes = [0_u8; 255];
    let length = match rustix::fs::fgetxattr(file, c"user.mime_type", &mut bytes[..]) {
        Ok(length) => length,
        Err(rustix::io::Errno::NODATA) => return Ok(String::new()),
        Err(_) => return Err(invalid_entry("Could not read clipboard MIME metadata")),
    };
    std::str::from_utf8(&bytes[..length])
        .map(str::to_owned)
        .map_err(|_| invalid_entry("Clipboard MIME metadata is not valid UTF-8"))
}

fn history_token(database: &DatabaseReader) -> u64 {
    let main = database.main();
    let favorites = database.favorites();
    history_token_from_parts(
        main.ring().write_head(),
        favorites.ring().write_head(),
        main.ring().len(),
        favorites.ring().len(),
    )
}

fn history_token_from_parts(
    main_head: u32,
    favorites_head: u32,
    main_len: u32,
    favorites_len: u32,
) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:history-token:v1:");
    for value in [main_head, favorites_head, main_len, favorites_len] {
        hasher.update(value.to_le_bytes());
    }
    let digest = hasher.finalize();
    let mut token = [0; 8];
    token.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(token)
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

fn entry_fingerprint(raw_id: u64, size: u64, mime: &str, bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:entry-fingerprint:v3:");
    hasher.update(raw_id.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mime.as_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

fn opaque_id(fingerprint: &[u8; 32]) -> String {
    format!("entry-{}", hex::encode(&fingerprint[..16]))
}

const MAX_SAFE_JSON_INTEGER: u64 = (1 << 53) - 1;

fn entry_revision(fingerprint: &[u8; 32]) -> u64 {
    let mut revision = [0; 8];
    revision.copy_from_slice(&fingerprint[16..24]);
    (u64::from_le_bytes(revision) & MAX_SAFE_JSON_INTEGER).max(1)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SAFE_JSON_INTEGER, SummaryCache, entry_fingerprint, entry_revision,
        history_token_from_parts, opaque_id,
    };

    #[test]
    fn summary_cache_is_retained_until_the_history_token_changes() {
        let mut cache = SummaryCache::default();
        cache.select_token(1);
        cache.entries.insert(42, None);
        cache.select_token(1);
        assert!(cache.entries.contains_key(&42));

        cache.select_token(2);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn every_history_component_changes_the_token() {
        let baseline = history_token_from_parts(1, 2, 3, 4);
        assert_eq!(baseline, history_token_from_parts(1, 2, 3, 4));
        for changed in [
            history_token_from_parts(9, 2, 3, 4),
            history_token_from_parts(1, 9, 3, 4),
            history_token_from_parts(1, 2, 9, 4),
            history_token_from_parts(1, 2, 3, 9),
        ] {
            assert_ne!(baseline, changed);
        }
    }

    #[test]
    fn engine_ids_are_not_exposed_and_revisions_are_stable() {
        let stable = entry_fingerprint(42, 3, "text/plain", b"abc");
        assert_eq!(stable, entry_fingerprint(42, 3, "text/plain", b"abc"));
        assert_ne!(stable, entry_fingerprint(43, 3, "text/plain", b"abc"));
        assert_ne!(stable, entry_fingerprint(42, 3, "text/plain", b"xyz"));

        let first = [0x2a; 32];
        let second = [0x2b; 32];
        assert!(opaque_id(&first).starts_with("entry-"));
        assert!(!opaque_id(&first).contains("42"));
        assert_eq!(opaque_id(&first), opaque_id(&first));
        assert_eq!(entry_revision(&first), entry_revision(&first));
        assert_ne!(entry_revision(&first), entry_revision(&second));
        assert!(entry_revision(&[u8::MAX; 32]) <= MAX_SAFE_JSON_INTEGER);
    }
}
