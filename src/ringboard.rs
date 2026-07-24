use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use std::os::unix::fs::MetadataExt;

use tokio::task::{JoinError, JoinHandle, spawn_blocking};

use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader};
use sha2::{Digest, Sha256};

use crate::{
    backend::{
        BackendError, BackendErrorKind, BackendMutation, BackendResult, ClipboardBackend,
        HistoryQuery, MAX_QUERY_LIMIT, ScreenshotRegion,
    },
    classification::{INSPECTION_LIMIT, bounded_preview, classify},
    model::{
        BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage, OperationResult,
    },
};

mod content;
mod mutation;

use content::{
    create_thumbnail, detail_facts, detected_image_mime, invalid_entry, prune_thumbnails,
    read_bounded,
};

const DEFAULT_MIME: &str = "text/plain";
const MAX_DETAILS_BYTES: usize = 256 * 1024;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 100;

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
    generation: u64,
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
}

impl Default for RingboardBackend {
    fn default() -> Self {
        Self {
            ids: Arc::new(Mutex::new(HashMap::new())),
            revision: Arc::new(Mutex::new(RevisionState::default())),
            summaries: Arc::new(Mutex::new(SummaryCache::default())),
            operations: Arc::new(Mutex::new(HashMap::new())),
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
        let token = history_token(&Self::open_database()?);
        self.ids
            .lock()
            .map_err(|_| lock_error())?
            .retain(|_, binding| binding.generation == token);
        Ok(token)
    }

    fn selected(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<(Entry, EntryReader, EntrySummary)> {
        let binding = self.resolve(opaque_id)?;
        let (database, mut reader) = Self::open()?;
        let generation = history_token(&database);
        if generation != binding.generation {
            self.ids.lock().map_err(|_| lock_error())?.clear();
            return Err(BackendError::stale(
                "Clipboard history changed after this entry was loaded",
            ));
        }
        let entry = database
            .get_raw(binding.raw_id)
            .map_err(|_| BackendError::stale("Clipboard entry is stale or missing"))?;
        let summary = self.summarize(entry, &mut reader, false, generation)?;
        if summary.id != opaque_id {
            self.ids.lock().map_err(|_| lock_error())?.remove(opaque_id);
            return Err(BackendError::stale(
                "Clipboard entry ID is stale or has been reused",
            ));
        }
        if summary.revision != binding.revision
            || expected_revision.is_some_and(|revision| revision != summary.revision)
        {
            self.ids.lock().map_err(|_| lock_error())?.remove(opaque_id);
            return Err(BackendError::stale(
                "Clipboard entry revision changed before the operation",
            ));
        }
        Ok((entry, reader, summary))
    }

    fn summarize(
        &self,
        entry: Entry,
        reader: &mut EntryReader,
        current: bool,
        generation: u64,
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
            .then(|| loaded.mime_type())
            .transpose()
            .map_err(|_| invalid_entry("Could not read clipboard MIME metadata"))?;
        let mime = detected_mime
            .or_else(|| {
                mime_value
                    .as_ref()
                    .map(|value| mime_or_default(value.as_str()))
            })
            .unwrap_or(DEFAULT_MIME);
        let fingerprint =
            entry_fingerprint(generation, entry.id(), byte_size, mime, &bytes, &metadata);
        let id = opaque_id(&fingerprint);
        Ok(EntrySummary {
            revision: entry_revision(&fingerprint),
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

    fn cached_summary(
        &self,
        cache: &mut SummaryCache,
        entry: Entry,
        reader: &mut EntryReader,
        current: bool,
        generation: u64,
    ) -> BackendResult<Option<EntrySummary>> {
        if let Some(summary) = cache.entries.get(&entry.id()) {
            return Ok(summary.clone());
        }
        let summary = match catch_unwind(AssertUnwindSafe(|| {
            self.summarize(entry, reader, current, generation)
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
            if let Some(summary) = self.cached_summary(
                &mut cache,
                entry,
                &mut reader,
                current_id == Some(raw_id),
                token,
            )? {
                current_ids.insert(
                    summary.id.clone(),
                    IdentityBinding {
                        raw_id,
                        generation: token,
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
        let token = history_token(&database);
        let entry = database
            .get_raw(raw_id)
            .map_err(|_| BackendError::not_found("Clipboard entry is stale or missing"))?;
        let summary = self.summarize(entry, &mut reader, false, token)?;
        self.ids.lock().map_err(|_| lock_error())?.insert(
            summary.id.clone(),
            IdentityBinding {
                raw_id,
                generation: token,
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
        let loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard image"))?;
        create_thumbnail(&loaded, &summary, edge)
    }

    fn mutate_sync(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        if !matches!(mutation, BackendMutation::Wipe | BackendMutation::Cleanup)
            && expected_revision.is_none()
        {
            return Err(BackendError::stale(
                "An expected clipboard entry revision is required",
            ));
        }
        match mutation {
            BackendMutation::Restore => self.restore_entry(opaque_id, expected_revision),
            BackendMutation::ImageAsFile => self.save_image_file(opaque_id, expected_revision),
            BackendMutation::Remove => self.remove_entry(opaque_id, expected_revision),
            BackendMutation::SetFavorite(value) => {
                self.move_entry(opaque_id, expected_revision, value)
            }
            BackendMutation::Wipe => self.wipe_entries(),
            BackendMutation::Cleanup => self.cleanup_artifacts(),
            BackendMutation::Annotate => Err(BackendError::new(
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
        self.restore_entry(&details.entry.id, Some(details.entry.revision))?;
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
        let backend = self.clone();
        run_blocking(move || backend.change_token_sync()).await
    }

    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let backend = self.clone();
        run_blocking(move || backend.query_sync(query)).await
    }

    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let backend = self.clone();
        let opaque_id = opaque_id.to_owned();
        run_blocking(move || backend.details_sync(&opaque_id, max_text_bytes)).await
    }

    async fn revision(&self, opaque_id: &str) -> BackendResult<u64> {
        let backend = self.clone();
        let opaque_id = opaque_id.to_owned();
        run_blocking(move || backend.revision_sync(&opaque_id)).await
    }

    async fn thumbnail(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let backend = self.clone();
        let opaque_id = opaque_id.to_owned();
        run_blocking(move || backend.thumbnail_sync(&opaque_id, expected_revision, edge)).await
    }

    async fn capture_screenshot(&self, region: ScreenshotRegion) -> BackendResult<OperationResult> {
        let backend = self.clone();
        run_blocking(move || backend.capture_region(region)).await
    }

    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        if !matches!(mutation, BackendMutation::Wipe | BackendMutation::Cleanup)
            && expected_revision.is_none()
        {
            return Err(BackendError::stale(
                "An expected clipboard entry revision is required",
            ));
        }
        if mutation == BackendMutation::Annotate {
            let backend = self.clone();
            let opaque_id = opaque_id.to_owned();
            let staged =
                run_blocking(move || backend.stage_annotation(&opaque_id, expected_revision))
                    .await?;
            return self.launch_annotation(staged);
        }
        let backend = self.clone();
        let opaque_id = opaque_id.to_owned();
        run_blocking(move || backend.mutate_sync(&opaque_id, expected_revision, mutation)).await
    }

    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntryDetails> {
        let backend = self.clone();
        let opaque_id = opaque_id.to_owned();
        let mime = mime.to_owned();
        let bytes = bytes.to_vec();
        run_blocking(move || backend.replace_sync(&opaque_id, expected_revision, &mime, &bytes))
            .await
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

fn entry_fingerprint(
    generation: u64,
    raw_id: u64,
    size: u64,
    mime: &str,
    bytes: &[u8],
    metadata: &std::fs::Metadata,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:entry-fingerprint:v1:");
    hasher.update(generation.to_le_bytes());
    hasher.update(raw_id.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mime.as_bytes());
    hasher.update(metadata.ino().to_le_bytes());
    hasher.update(metadata.ctime().to_le_bytes());
    hasher.update(metadata.ctime_nsec().to_le_bytes());
    hasher.update(bytes);
    hasher.finalize().into()
}

fn opaque_id(fingerprint: &[u8; 32]) -> String {
    format!("entry-{}", hex::encode(&fingerprint[..16]))
}

fn entry_revision(fingerprint: &[u8; 32]) -> u64 {
    let mut revision = [0; 8];
    revision.copy_from_slice(&fingerprint[16..24]);
    u64::from_le_bytes(revision)
}

#[cfg(test)]
mod tests {
    use super::{SummaryCache, entry_revision, history_token_from_parts, opaque_id};

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
        let first = [0x2a; 32];
        let second = [0x2b; 32];
        assert!(opaque_id(&first).starts_with("entry-"));
        assert!(!opaque_id(&first).contains("42"));
        assert_eq!(opaque_id(&first), opaque_id(&first));
        assert_eq!(entry_revision(&first), entry_revision(&first));
        assert_ne!(entry_revision(&first), entry_revision(&second));
    }
}
