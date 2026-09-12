use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::{
    sync::broadcast,
    task::{JoinHandle, spawn_blocking},
};

use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader, LoadedEntry};
use clipboard_history_core::{dirs::data_dir, protocol::RingKind};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    backend::{
        BackendError, BackendErrorKind, BackendMutation, BackendResult, ClipboardBackend,
        EntryTarget, FileSelection, HistoryQuery, MAX_QUERY_LIMIT, MAX_WAYLAND_SELECTION_BYTES,
        ScreenshotRegion,
    },
    classification::{INSPECTION_LIMIT, bounded_preview},
    editor::ImageEditorCommand,
    model::{
        BackendStatus, EntryDetails, EntrySummary, EntryThumbnail, HistoryPage, OperationResult,
        ReplacementResult,
    },
    selection::SelectionService,
};

mod artifacts;
#[cfg(all(test, feature = "benchmarks"))]
mod benchmarks;
mod content;
mod ipc;
mod mutation;
mod operation;
use operation::OperationControl;

use artifacts::ArtifactRegistry;
use content::{
    ResolvedContent, create_resolved_thumbnail, invalid_entry, prune_thumbnails, read_bounded,
};

const MAX_DETAILS_BYTES: usize = 256 * 1024;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 100;

macro_rules! run_backend {
    ($source:expr, $method:ident($($argument:expr),* $(,)?)) => {{
        let backend = $source.clone();
        run_blocking(move || {
            let _transaction = backend.transaction.lock().map_err(|_| lock_error())?;
            backend.$method($($argument),*)
        }).await
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
    projection: Option<CachedProjection>,
}

#[derive(Clone)]
struct ResolvedEntry {
    summary: EntrySummary,
    proof: [u8; 32],
    generated_paths: HashSet<PathBuf>,
    echo_source_id: Option<String>,
}

#[derive(Clone, Copy)]
struct IdentityBinding {
    raw_id: u64,
    revision: u64,
}

impl SummaryCache {
    fn select_token(&mut self, token: u64) {
        if self.token != Some(token) {
            self.projection = None;
        }
        self.token = Some(token);
    }
}

struct OperationTask {
    control: Arc<OperationControl>,
    handle: JoinHandle<()>,
    files: Vec<PathBuf>,
}

impl Drop for OperationTask {
    fn drop(&mut self) {
        mutation::remove_files(&self.files);
    }
}

struct QueryCandidate {
    raw_id: u64,
    resolved: ResolvedEntry,
}

struct CachedProjection {
    current_id: Option<u64>,
    candidates: Vec<QueryCandidate>,
    complete: bool,
}

#[derive(Default)]
struct QueryProjection {
    current: Option<EntrySummary>,
    entries: Vec<EntrySummary>,
    matched: usize,
    bindings: HashMap<String, IdentityBinding>,
    artifact_references: HashSet<PathBuf>,
    complete: bool,
}

impl QueryCandidate {
    fn collapsed_source(&self, collapse_echoes: bool, ids: &HashSet<&str>) -> Option<&str> {
        collapse_echoes
            .then_some(self.resolved.echo_source_id.as_deref())
            .flatten()
            .filter(|source| ids.contains(*source))
    }
}

impl QueryProjection {
    fn push(&mut self, candidate: &QueryCandidate, current_id: Option<&str>) {
        let summary = &candidate.resolved.summary;
        let binding = IdentityBinding {
            raw_id: candidate.raw_id,
            revision: summary.revision,
        };
        self.bindings.insert(summary.id.clone(), binding);
        if current_id == Some(summary.id.as_str()) {
            let mut current = summary.clone();
            current.current = true;
            self.current = Some(current);
        }
    }
}

impl CachedProjection {
    fn load(
        backend: &RingboardBackend,
        database: &DatabaseReader,
        reader: &mut EntryReader,
    ) -> BackendResult<Self> {
        let mut main = database.main().rev().peekable();
        let mut projection = Self {
            current_id: main.peek().map(Entry::id),
            candidates: Vec::new(),
            complete: true,
        };
        for entry in database.favorites().rev().chain(main) {
            match backend.read_summary(entry, reader)? {
                Some(resolved) => projection.candidates.push(QueryCandidate {
                    raw_id: entry.id(),
                    resolved,
                }),
                None => projection.complete = false,
            }
        }
        Ok(projection)
    }

    fn project(&self, query: &HistoryQuery) -> QueryProjection {
        let ids = self
            .candidates
            .iter()
            .map(|candidate| candidate.resolved.summary.id.as_str())
            .collect::<HashSet<_>>();
        let current_id = self
            .candidates
            .iter()
            .find(|candidate| self.current_id == Some(candidate.raw_id))
            .map(|candidate| {
                candidate
                    .collapsed_source(query.collapse_self_echoes, &ids)
                    .unwrap_or(&candidate.resolved.summary.id)
            });
        let mut projection = QueryProjection {
            artifact_references: self
                .candidates
                .iter()
                .flat_map(|candidate| candidate.resolved.generated_paths.iter().cloned())
                .collect(),
            complete: self.complete,
            ..QueryProjection::default()
        };
        let needle = query.query.trim().to_lowercase();
        let matches: Vec<_> = self
            .candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .collapsed_source(query.collapse_self_echoes, &ids)
                    .is_none()
            })
            .inspect(|candidate| projection.push(candidate, current_id))
            .map(|candidate| &candidate.resolved.summary)
            .filter(|summary| matches_query(summary, &needle))
            .collect();
        projection.matched = matches.len();
        projection.entries = matches
            .into_iter()
            .skip(query.offset)
            .take(query.limit.clamp(1, MAX_QUERY_LIMIT))
            .map(|summary| {
                let mut summary = summary.clone();
                summary.current = current_id == Some(summary.id.as_str());
                summary
            })
            .collect();
        projection
    }
}

#[derive(Clone)]
pub struct RingboardBackend {
    transaction: Arc<Mutex<()>>,
    operation_gate: Arc<tokio::sync::Mutex<()>>,
    ids: Arc<Mutex<HashMap<String, IdentityBinding>>>,
    revision: Arc<Mutex<RevisionState>>,
    summaries: Arc<Mutex<SummaryCache>>,
    operations: Arc<Mutex<HashMap<String, OperationTask>>>,
    operation_events: broadcast::Sender<OperationResult>,
    artifacts: Arc<Mutex<ArtifactRegistry>>,
    editor: ImageEditorCommand,
    selection: SelectionService,
}

impl Default for RingboardBackend {
    fn default() -> Self {
        let (operation_events, _) = broadcast::channel(64);
        Self {
            transaction: Arc::new(Mutex::new(())),
            operation_gate: Arc::new(tokio::sync::Mutex::new(())),
            ids: Arc::new(Mutex::new(HashMap::new())),
            revision: Arc::new(Mutex::new(RevisionState::default())),
            summaries: Arc::new(Mutex::new(SummaryCache::default())),
            operations: Arc::new(Mutex::new(HashMap::new())),
            operation_events,
            artifacts: Arc::new(Mutex::new(ArtifactRegistry::default())),
            editor: ImageEditorCommand::configured(),
            selection: SelectionService::default(),
        }
    }
}

impl RingboardBackend {
    fn open_database() -> BackendResult<DatabaseReader> {
        let mut directory = data_dir();
        DatabaseReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard history is unavailable"))
    }

    fn open() -> BackendResult<(DatabaseReader, EntryReader)> {
        let database = Self::open_database()?;
        let mut directory = data_dir();
        let reader = EntryReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard entries are unavailable"))?;
        Ok((database, reader))
    }

    fn artifact_registry(&self) -> BackendResult<MutexGuard<'_, ArtifactRegistry>> {
        self.artifacts.lock().map_err(|_| lock_error())
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
        history_token(&Self::open_database()?)
    }

    fn selected(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<(Entry, EntryReader, EntrySummary)> {
        self.selected_proven(opaque_id, expected_revision)
            .map(|(entry, reader, resolved)| (entry, reader, resolved.summary))
    }

    fn selected_proven(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<(Entry, EntryReader, ResolvedEntry)> {
        let binding = self.resolve(opaque_id)?;
        let (database, mut reader) = Self::open()?;
        let entry = database
            .get_raw(binding.raw_id)
            .map_err(|_| BackendError::stale("Clipboard entry is stale or missing"))?;
        // Storage slots (including bucket index and length) can be reused.
        // Never authorize an action against a cached content fingerprint.
        let resolved = self.summarize(entry, &mut reader)?;
        self.verify_selection(opaque_id, expected_revision, binding, &resolved.summary)?;
        Ok((entry, reader, resolved))
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

    fn summarize(&self, entry: Entry, reader: &mut EntryReader) -> BackendResult<ResolvedEntry> {
        let (mut loaded, metadata) = load_entry(entry, reader)?;
        let byte_size = metadata.len();
        let stored_mime = stored_mime_type(&loaded)?;
        let (bytes, content_digest) = inspect_entry(&mut *loaded, byte_size)?;
        let content = ResolvedContent::resolve(&stored_mime, &bytes, MAX_WAYLAND_SELECTION_BYTES);
        let fingerprint = entry_fingerprint(entry.id(), byte_size, content.mime(), &content_digest);
        let id = opaque_id(&fingerprint);
        let (generated_paths, echo_source_id, inline_echo_source) = {
            let registry = self.artifact_registry()?;
            loaded
                .seek(SeekFrom::Start(0))
                .map_err(|_| invalid_entry("Could not inspect artifact references"))?;
            let generated_paths = registry.references_in(BufReader::new(&mut *loaded))?;
            let echo_source_id = content
                .local_image()
                .and_then(|source| registry.match_local_image(source));
            let inline_echo_source =
                matches!(content.image(), Some(content::ResolvedImage::Inline { .. }))
                    .then(|| registry.match_inline_echo(content.mime(), &bytes, &id))
                    .flatten();
            (generated_paths, echo_source_id, inline_echo_source)
        };
        Ok(ResolvedEntry {
            summary: EntrySummary {
                revision: entry_revision(&fingerprint),
                id,
                kind: content.kind(),
                mime: content.mime().to_owned(),
                byte_size,
                favorite: entry.ring() == RingKind::Favorites,
                current: false,
                preview: bounded_preview(&bytes, INSPECTION_LIMIT),
            },
            proof: ipc::content_proof(&content_digest, &stored_mime),
            generated_paths,
            echo_source_id: echo_source_id.or(inline_echo_source),
        })
    }

    fn read_summary(
        &self,
        entry: Entry,
        reader: &mut EntryReader,
    ) -> BackendResult<Option<ResolvedEntry>> {
        let summary = match catch_unwind(AssertUnwindSafe(|| self.summarize(entry, reader))) {
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
        Ok(summary)
    }

    pub(super) fn generated_artifact_references(&self) -> BackendResult<HashSet<PathBuf>> {
        let (database, mut reader) = Self::open()?;
        let mut references = HashSet::new();
        for entry in database.favorites().chain(database.main()) {
            references.extend(self.summarize(entry, &mut reader)?.generated_paths);
        }
        Ok(references)
    }

    fn reconcile_projection(&self, projection: &QueryProjection) -> BackendResult<()> {
        if !projection.complete {
            return Ok(());
        }
        let result = self
            .artifacts
            .lock()
            .map_err(|_| lock_error())?
            .reconcile(&projection.artifact_references);
        if let Err(error) = result {
            tracing::warn!(code = %error.kind.code(), "Generated-file reconciliation failed");
        }
        Ok(())
    }

    fn query_sync(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let (database, mut reader) = Self::open()?;
        let token = history_token(&database)?;
        let projection = collect_query_projection(self, &database, &mut reader, &query, token)?;
        finalize_query(self, query, token, projection)
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
        let summary = self.summarize(entry, &mut reader)?.summary;
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
        expected_revision: Option<u64>,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let (entry, mut reader, summary) = self.selected(opaque_id, expected_revision)?;
        let mut loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard image"))?;
        let bytes = read_bounded(&mut loaded, INSPECTION_LIMIT)?;
        loaded
            .seek(SeekFrom::Start(0))
            .map_err(|_| invalid_entry("Could not rewind clipboard image"))?;
        let content = ResolvedContent::resolve(&summary.mime, &bytes, MAX_THUMBNAIL_BYTES);
        create_resolved_thumbnail(&loaded, &content, &summary, edge)
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
                "External editing must be started asynchronously",
            )),
        }
    }

    fn replace_sync(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        let raw_id = self.replace_entry(opaque_id, Some(expected_revision), mime, bytes)?;
        let details = self.details_raw_sync(raw_id, MAX_DETAILS_BYTES)?;
        let publication = self.restore_entry(
            &details.entry.id,
            Some(details.entry.revision),
            MAX_DETAILS_BYTES as u64,
        );
        let (selection_published, publication_message) = match publication {
            Ok(_) => (true, "Replacement published to the clipboard".to_owned()),
            Err(error) => {
                // The Ringboard replacement has already committed. Report the
                // publication failure as partial success rather than claiming
                // that the edit itself failed.
                let _ = self.clear_identity_state();
                (
                    false,
                    format!("Replacement committed, but clipboard publication failed: {error}"),
                )
            }
        };
        Ok(ReplacementResult {
            entry: details,
            selection_published,
            publication_message,
        })
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
        cache.projection = None;
        Ok(())
    }

    fn publish_sync(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        if bytes.is_empty() {
            return Err(invalid_data(
                "Published clipboard content must not be empty",
            ));
        }
        self.selection.publish(mime, bytes, max_bytes)?;
        Ok(OperationResult::completed(
            "publish",
            "Content published to the Wayland clipboard",
        ))
    }

    fn publish_files_sync(
        &self,
        file_selection: FileSelection,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let file_count = file_selection.paths.len();
        if !(1..=MAX_FILES).contains(&file_count) {
            return Err(invalid_data(
                "File selection must contain between 1 and 100 paths",
            ));
        }

        let uri_list = encode_file_uris(&file_selection.paths)?;
        self.selection
            .publish_files(file_selection.operation.as_str(), uri_list, max_bytes)?;
        Ok(OperationResult::completed(
            "publish-files",
            &format!(
                "Mirrored {file_count} {} to the clipboard",
                file_label(file_count)
            ),
        ))
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

fn collect_query_projection(
    backend: &RingboardBackend,
    database: &DatabaseReader,
    reader: &mut EntryReader,
    query: &HistoryQuery,
    token: u64,
) -> BackendResult<QueryProjection> {
    let mut cache = backend.summaries.lock().map_err(|_| lock_error())?;
    cache.select_token(token);
    if let Some(projection) = &cache.projection {
        return Ok(projection.project(query));
    }
    let projection = CachedProjection::load(backend, database, reader)?;
    if history_token(database)? != token {
        cache.projection = None;
        return Err(BackendError::stale(
            "History changed while it was being read; retry the query",
        ));
    }
    let result = projection.project(query);
    // Retry unreadable entries on the next query rather than caching an outage.
    if projection.complete {
        cache.projection = Some(projection);
    }
    Ok(result)
}

fn finalize_query(
    backend: &RingboardBackend,
    query: HistoryQuery,
    token: u64,
    mut projection: QueryProjection,
) -> BackendResult<HistoryPage> {
    prune_thumbnails(
        projection
            .bindings
            .iter()
            .map(|(id, binding)| (id.as_str(), binding.revision)),
    );
    *backend.ids.lock().map_err(|_| lock_error())? = std::mem::take(&mut projection.bindings);
    backend.reconcile_projection(&projection)?;
    let consumed = query.offset.saturating_add(projection.entries.len());
    let has_more = projection.matched > consumed;
    Ok(HistoryPage {
        revision: backend.revision_for(token)?,
        generation: query.generation,
        current: projection.current,
        has_more,
        next_offset: has_more.then_some(consumed),
        entries: projection.entries,
    })
}

#[async_trait]
impl ClipboardBackend for RingboardBackend {
    fn operation_events(&self) -> broadcast::Receiver<OperationResult> {
        self.operation_events.subscribe()
    }

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
        expected_revision: Option<u64>,
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

    async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let mime = mime.to_owned();
        run_backend!(self, publish_sync(&mime, bytes, max_bytes))
    }

    async fn publish_files(
        &self,
        selection: FileSelection,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        run_backend!(self, publish_files_sync(selection, max_bytes))
    }

    async fn mutate(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        mutation.require_revision(expected_revision)?;
        // Exclude new annotation launches while cleanup/wipe drains old jobs.
        let _gate = self.operation_gate.lock().await;
        if matches!(mutation, BackendMutation::Wipe | BackendMutation::Cleanup) {
            self.stop_operations().await?;
        }
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

    async fn remove_many(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        let targets = targets.to_vec();
        run_backend!(self, remove_entries(&targets))
    }

    async fn replace(
        &self,
        opaque_id: &str,
        expected_revision: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        let opaque_id = opaque_id.to_owned();
        let mime = mime.to_owned();
        let bytes = bytes.to_vec();
        run_backend!(
            self,
            replace_sync(&opaque_id, expected_revision, &mime, &bytes)
        )
    }

    async fn cancel_operation(&self, operation_id: &str) -> BackendResult<bool> {
        let operation = {
            let mut operations = self.operations.lock().map_err(|_| lock_error())?;
            if !operations
                .get(operation_id)
                .is_some_and(|operation| operation.control.cancel())
            {
                // A committing job cannot be aborted. It retains its files and
                // emits its real committed/failed outcome; callers may wait.
                return Ok(false);
            }
            operations.remove(operation_id)
        };
        let Some(mut operation) = operation else {
            return Ok(false);
        };
        operation.handle.abort();
        let _ = (&mut operation.handle).await;
        let control = Arc::clone(&operation.control);
        drop(operation);
        let _ = self.operation_events.send(OperationResult::with_id(
            operation_id.to_owned(),
            "annotate",
            "cancelled",
            "Image edit cancelled",
        ));
        control.finish();
        Ok(true)
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
    let content = ResolvedContent::resolve(&summary.mime, &bytes, MAX_THUMBNAIL_BYTES);
    Ok(EntryDetails {
        entry: summary,
        text,
        files: content.files().to_vec(),
        image: content.image_metadata(),
        preview_truncated: truncated,
    })
}

async fn run_blocking<T>(
    work: impl FnOnce() -> BackendResult<T> + Send + 'static,
) -> BackendResult<T>
where
    T: Send + 'static,
{
    spawn_blocking(work)
        .await
        .map_err(|_| operation_failed("Clipboard backend task failed"))?
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RingFileState {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

impl RingFileState {
    fn load(path: &Path) -> BackendResult<Self> {
        let metadata = fs::metadata(path)
            .map_err(|_| BackendError::unavailable("Ringboard history metadata is unavailable"))?;
        Ok(Self::from_metadata(&metadata))
    }

    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            size: metadata.size(),
            modified_seconds: metadata.mtime(),
            modified_nanoseconds: metadata.mtime_nsec(),
            changed_seconds: metadata.ctime(),
            changed_nanoseconds: metadata.ctime_nsec(),
        }
    }

    fn hash_into(self, hasher: &mut Sha256) {
        hasher.update(self.device.to_le_bytes());
        hasher.update(self.inode.to_le_bytes());
        hasher.update(self.size.to_le_bytes());
        hasher.update(self.modified_seconds.to_le_bytes());
        hasher.update(self.modified_nanoseconds.to_le_bytes());
        hasher.update(self.changed_seconds.to_le_bytes());
        hasher.update(self.changed_nanoseconds.to_le_bytes());
    }
}

fn history_token(database: &DatabaseReader) -> BackendResult<u64> {
    let main = database.main();
    let favorites = database.favorites();
    let directory = data_dir();
    Ok(history_token_from_parts(
        main.ring().write_head(),
        favorites.ring().write_head(),
        main.ring().len(),
        favorites.ring().len(),
        RingFileState::load(&directory.join(RingKind::Main.file_name()))?,
        RingFileState::load(&directory.join(RingKind::Favorites.file_name()))?,
    ))
}

fn history_token_from_parts(
    main_head: u32,
    favorites_head: u32,
    main_len: u32,
    favorites_len: u32,
    main_file: RingFileState,
    favorites_file: RingFileState,
) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:history-token:v2:");
    for value in [main_head, favorites_head, main_len, favorites_len] {
        hasher.update(value.to_le_bytes());
    }
    main_file.hash_into(&mut hasher);
    favorites_file.hash_into(&mut hasher);
    let digest = hasher.finalize();
    let mut token = [0; 8];
    token.copy_from_slice(&digest[..8]);
    u64::from_le_bytes(token)
}

fn load_entry(
    entry: Entry,
    reader: &mut EntryReader,
) -> BackendResult<(LoadedEntry<'_, File>, fs::Metadata)> {
    let loaded = entry
        .to_file(reader)
        .map_err(|_| invalid_entry("Could not open clipboard entry"))?;
    let metadata = loaded
        .metadata()
        .map_err(|_| invalid_entry("Could not read clipboard entry metadata"))?;
    Ok((loaded, metadata))
}

fn encode_file_uris(paths: &[PathBuf]) -> BackendResult<Vec<u8>> {
    paths.iter().try_fold(Vec::new(), |mut bytes, path| {
        if !path.is_absolute() {
            return Err(invalid_data("File selection paths must be absolute"));
        }
        let uri = Url::from_file_path(path)
            .map_err(|_| invalid_data("File selection path is invalid"))?;
        bytes.extend_from_slice(uri.as_str().as_bytes());
        bytes.extend_from_slice(b"\r\n");
        Ok(bytes)
    })
}

fn file_label(count: usize) -> &'static str {
    if count == 1 { "file" } else { "files" }
}

fn invalid_data(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}

fn matches_query(summary: &EntrySummary, needle: &str) -> bool {
    needle.is_empty()
        || summary.preview.to_lowercase().contains(needle)
        || summary.mime.to_lowercase().contains(needle)
}

fn lock_error() -> BackendError {
    operation_failed("Clipboard backend state is unavailable")
}

fn operation_failed(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, message)
}

fn inspect_entry(source: &mut impl Read, expected_size: u64) -> BackendResult<(Vec<u8>, [u8; 32])> {
    let preview_capacity = usize::try_from(expected_size)
        .unwrap_or(usize::MAX)
        .min(INSPECTION_LIMIT);
    let mut preview = Vec::with_capacity(preview_capacity);
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:entry-content:v1:");
    let read_error = |_| invalid_entry("Could not read clipboard entry");
    source
        .by_ref()
        .take(INSPECTION_LIMIT as u64)
        .read_to_end(&mut preview)
        .map_err(read_error)?;
    hasher.update(&preview);
    let remaining = std::io::copy(source, &mut hasher).map_err(read_error)?;
    let actual_size = remaining
        .checked_add(preview.len() as u64)
        .ok_or_else(|| invalid_entry("Clipboard entry size is invalid"))?;
    if actual_size != expected_size {
        return Err(BackendError::stale(
            "Clipboard entry changed while its identity was calculated",
        ));
    }
    Ok((preview, hasher.finalize().into()))
}

fn entry_fingerprint(raw_id: u64, size: u64, mime: &str, content_digest: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:entry-fingerprint:v4:");
    hasher.update(raw_id.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mime.as_bytes());
    hasher.update(content_digest);
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
        CachedProjection, MAX_SAFE_JSON_INTEGER, QueryCandidate, ResolvedEntry, entry_fingerprint,
        entry_revision, inspect_entry, opaque_id,
    };
    use crate::{
        backend::HistoryQuery,
        model::{EntryKind, EntrySummary},
    };

    fn query(
        needle: &str,
        offset: usize,
        limit: usize,
        collapse_self_echoes: bool,
    ) -> HistoryQuery {
        HistoryQuery {
            query: needle.into(),
            generation: 1,
            offset,
            limit,
            collapse_self_echoes,
        }
    }

    fn candidate(raw_id: u64, id: &str, generated: bool) -> QueryCandidate {
        QueryCandidate {
            raw_id,
            resolved: ResolvedEntry {
                summary: EntrySummary {
                    id: id.into(),
                    revision: 1,
                    kind: EntryKind::Image,
                    mime: "image/png".into(),
                    byte_size: 10,
                    favorite: false,
                    current: false,
                    preview: "image".into(),
                },
                proof: [0; 32],
                generated_paths: generated
                    .then(|| "/generated/image.png".into())
                    .into_iter()
                    .collect(),
                echo_source_id: generated.then(|| "source".into()),
            },
        }
    }

    #[test]
    fn cached_projection_preserves_identity_and_current_outside_the_page() {
        let cached = CachedProjection {
            current_id: Some(2),
            complete: false,
            candidates: vec![candidate(2, "echo", true), candidate(1, "source", false)],
        };
        for (request, expected, matched, current) in [
            (query("", 0, 10, false), vec!["echo", "source"], 2, "echo"),
            (query("", 0, 10, true), vec!["source"], 1, "source"),
            (query(" IMAGE/PNG ", 1, 1, false), vec!["source"], 2, "echo"),
            (query("missing", 0, 10, true), vec![], 0, "source"),
            (query("", usize::MAX, 10, true), vec![], 1, "source"),
            (query("", 0, 0, true), vec!["source"], 1, "source"),
        ] {
            let page = cached.project(&request);
            assert_eq!(
                page.entries
                    .iter()
                    .map(|entry| entry.id.as_str())
                    .collect::<Vec<_>>(),
                expected
            );
            assert_eq!(page.matched, matched);
            assert_eq!(page.current.as_ref().unwrap().id, current);
            assert!(page.current.as_ref().unwrap().current);
            assert_eq!(page.bindings["source"].raw_id, 1);
            assert_eq!(
                page.bindings.contains_key("echo"),
                !request.collapse_self_echoes
            );
            assert!(
                page.entries
                    .iter()
                    .all(|entry| entry.current == (entry.id == current))
            );
            assert!(
                page.artifact_references
                    .contains(std::path::Path::new("/generated/image.png"))
            );
            assert!(!page.complete);
        }
        assert!(
            cached
                .candidates
                .iter()
                .all(|candidate| !candidate.resolved.summary.current)
        );
        let orphan = CachedProjection {
            candidates: vec![candidate(2, "echo", true)],
            ..cached
        };
        assert_eq!(
            orphan.project(&query("", 0, 10, true)).entries[0].id,
            "echo"
        );
    }

    fn fingerprint(raw_id: u64, mime: &str, bytes: &[u8]) -> [u8; 32] {
        let (_, digest) = inspect_entry(&mut std::io::Cursor::new(bytes), bytes.len() as u64)
            .expect("fingerprint fixture");
        entry_fingerprint(raw_id, bytes.len() as u64, mime, &digest)
    }

    struct InterruptOnce<R>(bool, R);

    impl<R: std::io::Read> std::io::Read for InterruptOnce<R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.0) {
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.1.read(buffer)
        }
    }

    #[test]
    fn inspection_is_bounded_and_hashes_the_full_stream_across_interruptions() {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let limit = crate::classification::INSPECTION_LIMIT;
        for size in [0, 1, limit - 1, limit, limit + 1, limit + 8193] {
            let bytes = vec![b'x'; size];
            let split = size.min(limit);
            let tail = InterruptOnce(true, &bytes[split..]);
            let mut source = InterruptOnce(true, (&bytes[..split]).chain(tail));
            let (preview, digest) = inspect_entry(&mut source, size as u64).unwrap();
            assert_eq!(preview, bytes[..split]);
            let mut expected = Sha256::new();
            expected.update(b"clip-daemon:entry-content:v1:");
            expected.update(&bytes);
            assert_eq!(digest.as_slice(), expected.finalize().as_slice());
        }
    }

    #[test]
    fn entry_identity_covers_full_content_and_preserves_js_safe_revisions() {
        let mut first = vec![b'a'; crate::classification::INSPECTION_LIMIT + 1];
        let mut second = first.clone();
        first[crate::classification::INSPECTION_LIMIT] = b'x';
        second[crate::classification::INSPECTION_LIMIT] = b'y';

        let identity = fingerprint(42, "text/plain", &first);
        let changed = fingerprint(42, "text/plain", &second);
        assert_ne!(identity, changed);
        assert_eq!(identity, fingerprint(42, "text/plain", &first));
        assert_ne!(identity, fingerprint(43, "text/plain", &first));
        assert_ne!(opaque_id(&identity), opaque_id(&changed));
        assert_ne!(entry_revision(&identity), entry_revision(&changed));
        assert!(entry_revision(&[u8::MAX; 32]) <= MAX_SAFE_JSON_INTEGER);
        assert!(inspect_entry(&mut &b"abc"[..], 2).is_err());
        assert!(inspect_entry(&mut &b"abc"[..], 4).is_err());
    }
}
