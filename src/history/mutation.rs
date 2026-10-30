use super::{
    HistoryBackend,
    content::{MAX_THUMBNAIL_BYTES, ResolvedContent, ResolvedImage, valid_edited_image},
    operation::{
        OperationCompletion, OperationControl, private_directory, private_file, runtime_directory,
        unique_path,
    },
    run_blocking,
};
use crate::{
    backend::{BackendResult, EntryTarget, invalid_data as invalid_entry, operation_error},
    model::{EntryKind, OperationResult},
    selection::{effective_limit, read_path, validate_size},
};
use std::{
    borrow::Cow,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use url::Url;

pub(super) struct AnnotationStage {
    input: PathBuf,
    output: PathBuf,
    opaque_id: String,
    revision: u64,
    max_bytes: u64,
}
impl HistoryBackend {
    fn track_image_file(
        &self,
        path: &Path,
        created: bool,
        source_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        if !created {
            return Ok(());
        }
        self.artifact_registry()?
            .register(path, source_id, mime, bytes)
            .inspect_err(|_| {
                let _ = fs::remove_file(path);
            })
    }
    fn rollback_image_file(&self, path: &Path, created: bool) -> BackendResult<()> {
        if created {
            self.artifact_registry()?.remove_registered(path)?;
        }
        Ok(())
    }
    pub(super) fn restore_entry(
        &self,
        id: &str,
        expected: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_bytes(self, id, expected, max_bytes)?;
        publish_entry(self, &summary, bytes, max_bytes)?;
        self.selection_changed()?;
        Ok(OperationResult::completed(
            "copy",
            "Entry published to the Wayland clipboard",
        ))
    }
    pub(super) fn selection_changed(&self) -> BackendResult<()> {
        self.artifact_registry()?.clear_active_selection()
    }
    pub(super) fn save_image_file(
        &self,
        id: &str,
        expected: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_image(self, id, expected, max_bytes)?;
        let content = resolved_content(&summary.mime, &bytes, max_bytes);
        let path = prepare_image_file(&content, &bytes)?;
        let created = matches!(path, Cow::Owned(_));
        self.track_image_file(&path, created, &summary.id, content.mime(), &bytes)?;
        if let Err(error) = publish_image_uri(self, &path, max_bytes) {
            self.rollback_image_file(&path, created)?;
            return Err(error);
        }
        self.artifact_registry()?.activate_if_generated(&path)?;
        let mut result = OperationResult::completed(
            "image-as-file",
            if created {
                "Image file copied"
            } else {
                "Image file link copied"
            },
        );
        result.path = Some(path.to_string_lossy().into_owned());
        Ok(result)
    }
    pub(super) fn launch_annotation(
        &self,
        staged: AnnotationStage,
    ) -> BackendResult<OperationResult> {
        let backend = self.clone();
        self.operations.launch(
            "annotate",
            "Image editor started",
            vec![staged.input.clone(), staged.output.clone()],
            move |control| run_annotation(backend, staged, control),
            |_| async {},
        )
    }
    pub(super) fn stage_annotation(
        &self,
        id: &str,
        expected: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<AnnotationStage> {
        let limit = max_bytes.min(MAX_THUMBNAIL_BYTES);
        let (summary, bytes) = selected_image(self, id, expected, limit)?;
        let content = resolved_content(&summary.mime, &bytes, limit);
        let (mime, image_bytes) = match content.image() {
            Some(ResolvedImage::Inline { mime, .. }) => (*mime, bytes),
            Some(ResolvedImage::LocalFile(source)) => {
                (source.mime, read_path(&source.path, limit)?)
            }
            None => return Err(invalid_entry("Only image entries support this action")),
        };
        let directory = runtime_directory("clip-daemon/edits")?;
        let input = unique_path(&directory, image_extension(mime));
        let output = unique_path(&directory, "png");
        write_private(&input, &image_bytes)?;
        Ok(AnnotationStage {
            input,
            output,
            opaque_id: id.to_owned(),
            revision: summary.revision,
            max_bytes: limit,
        })
    }
    pub(super) fn remove_entry(
        &self,
        id: &str,
        expected: Option<u64>,
    ) -> BackendResult<OperationResult> {
        let selected = self.selected(id, expected)?;
        self.store()?.remove_many(&[EntryTarget {
            opaque_id: id.to_owned(),
            expected_revision: selected.summary.revision,
        }])?;
        Ok(OperationResult::completed(
            "delete",
            "Clipboard entry deleted",
        ))
    }
    pub(super) fn remove_entries(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        // The worker validates every target and deletes in one SQL transaction.
        self.store()?.remove_many(targets)?;
        let count = targets.len();
        Ok(OperationResult::completed(
            "delete-many",
            &format!(
                "{count} clipboard {} deleted",
                if count == 1 { "entry" } else { "entries" }
            ),
        ))
    }
    pub(super) fn move_entry(
        &self,
        id: &str,
        expected: Option<u64>,
        favorite: bool,
    ) -> BackendResult<OperationResult> {
        let selected = self.selected(id, expected)?;
        self.store()?
            .favorite(id, selected.summary.revision, favorite)?;
        Ok(OperationResult::completed(
            if favorite { "favorite" } else { "unfavorite" },
            "Favorite state updated",
        ))
    }
    pub(super) fn cleanup_artifacts(&self) -> BackendResult<OperationResult> {
        let removed = cleanup_backend(self)?;
        Ok(OperationResult::completed(
            "cleanup",
            &format!("Clipboard caches cleared; {removed} unreferenced generated files removed"),
        ))
    }
    pub(super) fn wipe_entries(&self) -> BackendResult<OperationResult> {
        self.store()?.wipe()?;
        // History is committed even if optional filesystem cleanup fails.
        if let Err(error) =
            cleanup_backend(self).and_then(|_| self.artifact_registry()?.clear_all())
        {
            tracing::warn!(%error, "History wiped; artifact cleanup deferred");
        }
        Ok(OperationResult::completed(
            "wipe",
            "Clipboard history cleared",
        ))
    }
}
fn cleanup_backend(backend: &HistoryBackend) -> BackendResult<usize> {
    super::content::clear_cache()?;
    fs::remove_dir_all(runtime_directory("clip-daemon")?).map_err(operation_error)?;
    backend.artifact_registry()?.reconcile()
}
async fn run_annotation(
    backend: HistoryBackend,
    staged: AnnotationStage,
    control: Arc<OperationControl>,
) -> BackendResult<OperationCompletion> {
    let AnnotationStage {
        input,
        output,
        opaque_id,
        revision,
        max_bytes,
    } = staged;
    tokio::time::sleep(Duration::from_millis(150)).await;
    backend.operations.edit(&input, &output).await?;
    if !output.is_file() {
        return Err(operation_error("Image edit was cancelled"));
    }
    if !control.begin_commit() {
        return Err(operation_error("Image edit was cancelled before commit"));
    }
    run_blocking(move || {
        let _transaction = backend
            .transaction
            .lock()
            .map_err(|_| super::lock_error())?;
        apply_annotation(&backend, &opaque_id, revision, &output, max_bytes)
    })
    .await
}
fn apply_annotation(
    backend: &HistoryBackend,
    id: &str,
    revision: u64,
    output: &Path,
    max_bytes: u64,
) -> BackendResult<OperationCompletion> {
    if !valid_edited_image(output, max_bytes) {
        return Err(operation_error("Annotation returned an invalid image"));
    }
    let bytes = read_path(output, max_bytes)?;
    backend
        .store()?
        .replace(id, revision, "image/png", &bytes)?;
    let mut completion = OperationCompletion {
        status: "completed",
        message: "Image edit completed",
        warning: None,
    };
    // Capture deduplicates against the committed image using SQL MIME/content identity.
    if let Err(error) = backend
        .selection
        .publish("image/png", bytes, max_bytes)
        .and_then(|_| backend.selection_changed())
    {
        completion.message = "Image edit committed";
        completion.warning = Some(format!("Clipboard publication failed: {error}"));
    }
    Ok(completion)
}
fn selected_bytes(
    backend: &HistoryBackend,
    id: &str,
    expected: Option<u64>,
    max_bytes: u64,
) -> BackendResult<(crate::model::EntrySummary, Vec<u8>)> {
    let mut stored = backend.selected(id, expected)?;
    validate_size(stored.bytes.len() as u64, max_bytes)?;
    // Internal content resolution needs the captured MIME, not the semantic UI
    // label (a file URI can have an image/png summary but still contain a URI).
    stored.summary.mime = stored.mime;
    Ok((stored.summary, stored.bytes))
}
fn selected_image(
    backend: &HistoryBackend,
    id: &str,
    expected: Option<u64>,
    max_bytes: u64,
) -> BackendResult<(crate::model::EntrySummary, Vec<u8>)> {
    let selected = selected_bytes(backend, id, expected, max_bytes)?;
    (selected.0.kind == EntryKind::Image)
        .then_some(selected)
        .ok_or_else(|| invalid_entry("Only image entries support this action"))
}
fn resolved_content<'a>(mime: &'a str, bytes: &[u8], max_bytes: u64) -> ResolvedContent<'a> {
    ResolvedContent::resolve(mime, bytes, effective_limit(max_bytes))
}
fn publish_entry(
    backend: &HistoryBackend,
    summary: &crate::model::EntrySummary,
    bytes: Vec<u8>,
    max_bytes: u64,
) -> BackendResult<()> {
    let content = resolved_content(&summary.mime, &bytes, max_bytes);
    if summary.kind == EntryKind::Image && content.kind() != EntryKind::Image {
        return Err(invalid_entry(
            "Clipboard image file is missing, unsafe, invalid, or exceeds the size limit",
        ));
    }
    match content.local_image() {
        Some(source) => backend
            .selection
            .publish_file(source.mime, &source.path, max_bytes),
        None => backend.selection.publish(content.mime(), bytes, max_bytes),
    }
}
fn prepare_image_file<'a>(
    content: &'a ResolvedContent<'_>,
    bytes: &[u8],
) -> BackendResult<Cow<'a, Path>> {
    match content.image() {
        Some(ResolvedImage::LocalFile(source)) => Ok(Cow::Borrowed(&source.path)),
        Some(ResolvedImage::Inline { mime, .. }) => {
            let path = unique_path(&image_directory()?, image_extension(mime));
            write_private(&path, bytes)?;
            Ok(Cow::Owned(path))
        }
        None => Err(invalid_entry("Only image entries support this action")),
    }
}
fn publish_image_uri(backend: &HistoryBackend, path: &Path, max_bytes: u64) -> BackendResult<()> {
    let uri = Url::from_file_path(path)
        .map_err(|_| operation_error("Could not create image file URI"))?;
    backend
        .selection
        .publish_file_link(format!("{uri}\r\n").into_bytes(), max_bytes)
}
fn image_directory() -> BackendResult<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| operation_error("HOME is unavailable"))?;
    private_directory(PathBuf::from(home).join("Pictures/Screenshots/clipboard-history"))
}
fn write_private(path: &Path, bytes: &[u8]) -> BackendResult<()> {
    let mut file = private_file(path)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(operation_error)
        .inspect_err(|_| {
            let _ = fs::remove_file(path);
        })
}
fn image_extension(mime: &str) -> &'static str {
    match mime.split(';').next().unwrap_or(mime) {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "image/svg+xml" => "svg",
        _ => "png",
    }
}
#[cfg(test)]
mod tests {
    #[test]
    fn local_image_actions_use_captured_mime_not_the_semantic_label() {
        use crate::history_store::{HistoryStore, StorePolicy};
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image.png");
        image::RgbaImage::new(2, 3).save(&image).unwrap();
        let uri = format!("{}\r\n", url::Url::from_file_path(&image).unwrap());
        let store = HistoryStore::open(&directory.path().join("store/history.db")).unwrap();
        store
            .apply_policy(StorePolicy {
                admission: true,
                ..Default::default()
            })
            .unwrap();
        let entry = store.insert("text/uri-list", uri.as_bytes()).unwrap();
        assert_eq!(entry.mime, "image/png");
        let backend = super::HistoryBackend::from_store(store.clone());
        let (internal, bytes) =
            super::selected_image(&backend, &entry.id, Some(entry.revision), 1024).unwrap();
        assert_eq!(internal.mime, "text/uri-list");
        assert_eq!(bytes, uri.as_bytes());
        let error =
            super::selected_image(&backend, &entry.id, Some(entry.revision), 1).unwrap_err();
        assert_eq!(error.kind, crate::backend::BackendErrorKind::InvalidData);
        let content = super::resolved_content(&internal.mime, &bytes, 1024);
        assert_eq!(content.local_image().unwrap().path, image);
        let details =
            crate::history::content::entry_details(store.get(&entry.id, None).unwrap(), 1024)
                .unwrap();
        assert_eq!(details.image.unwrap().height, 3);
        assert_eq!(details.files.len(), 1);
        assert_eq!(details.entry.mime, "image/png");
        store.shutdown().unwrap();
    }

    #[test]
    fn staging_never_overwrites_or_removes_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("edit");
        super::write_private(&path, b"original").unwrap();
        assert!(super::write_private(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }
}
