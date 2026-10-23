use std::{
    borrow::Cow,
    env,
    fs::{self, File},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use url::Url;

use crate::{
    backend::{
        BackendResult, EntryTarget, MAX_WAYLAND_SELECTION_BYTES, invalid_data as invalid_entry,
        operation_error,
    },
    model::{EntryKind, OperationResult},
    selection::read_path,
};

use super::{
    RingboardBackend,
    content::{
        MAX_THUMBNAIL_BYTES, ResolvedContent, ResolvedImage, read_entry, valid_edited_image,
    },
    operation::{
        OperationCompletion, OperationControl, private_directory, private_file, runtime_directory,
        unique_path,
    },
    run_blocking,
};

pub(super) struct AnnotationStage {
    input: PathBuf,
    output: PathBuf,
    opaque_id: String,
    revision: u64,
    max_bytes: u64,
}

impl RingboardBackend {
    fn track_image_file(
        &self,
        path: &Path,
        created: bool,
        source_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        if !created {
            self.artifact_registry()?.activate_if_generated(path);
            return Ok(());
        }
        self.artifact_registry()
            .and_then(|mut registry| registry.register(path, source_id, mime, bytes))
            .inspect_err(|_| {
                let _ = fs::remove_file(path);
            })
    }

    fn rollback_image_file(&self, path: &Path, created: bool) -> BackendResult<()> {
        if created {
            self.artifact_registry()?.forget(path);
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    pub(super) fn restore_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_bytes(self, opaque_id, expected_revision, max_bytes)?;
        publish_entry(self, &summary, bytes, max_bytes)?;
        self.selection_changed()?;
        Ok(OperationResult::completed(
            "copy",
            "Entry published to the Wayland clipboard",
        ))
    }

    pub(super) fn selection_changed(&self) -> BackendResult<()> {
        self.artifact_registry()?.clear_active_selection();
        self.clear_identity_state()
    }

    pub(super) fn save_image_file(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_image(self, opaque_id, expected_revision, max_bytes)?;
        let content = resolved_content(&summary.mime, &bytes, max_bytes);
        let path = prepare_image_file(&content, &bytes)?;
        let created = matches!(path, Cow::Owned(_));
        self.track_image_file(&path, created, &summary.id, content.mime(), &bytes)?;
        if let Err(error) = publish_image_uri(self, &path, max_bytes) {
            self.rollback_image_file(&path, created)?;
            return Err(error);
        }
        self.clear_identity_state()?;
        let message = ["Image file link copied", "Image file copied"][usize::from(created)];
        let mut result = OperationResult::completed("image-as-file", message);
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
        opaque_id: &str,
        expected_revision: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<AnnotationStage> {
        let limit = max_bytes.min(MAX_THUMBNAIL_BYTES);
        let (summary, bytes) = selected_image(self, opaque_id, expected_revision, limit)?;
        let revision = summary.revision;
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
            opaque_id: opaque_id.to_owned(),
            revision,
            max_bytes: limit,
        })
    }

    fn replace_file_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        path: &Path,
        mime: &str,
    ) -> BackendResult<u64> {
        let (entry, _, resolved) = self.selected_proven(opaque_id, expected_revision)?;
        let file = File::open(path).map_err(operation_error)?;
        // Mirror Ringboard Add normalization. Otherwise publishing a text edit
        // creates a second, MIME-distinct capture instead of deduplicating it.
        super::ipc::replace(
            entry.id(),
            &resolved.proof,
            super::storage_mime(mime),
            &file,
        )?;
        self.clear_identity_state()?;
        Ok(entry.id())
    }

    pub(super) fn replace_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<u64> {
        let directory = runtime_directory("clip-daemon/transfers")?;
        let path = unique_path(&directory, "edit");
        write_private(&path, bytes)?;
        let result = self.replace_file_entry(opaque_id, expected_revision, &path, mime);
        let _ = fs::remove_file(path);
        result
    }

    pub(super) fn remove_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<OperationResult> {
        let (entry, _, resolved) = self.selected_proven(opaque_id, expected_revision)?;
        super::ipc::remove(entry.id(), &resolved.proof)?;
        self.clear_identity_state()?;
        Ok(OperationResult::completed(
            "delete",
            "Clipboard entry deleted",
        ))
    }

    pub(super) fn remove_entries(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        // Resolve and validate the complete selection before mutating history so
        // a stale row cannot turn a bulk request into an avoidable partial delete.
        let proven = targets
            .iter()
            .map(|target| {
                self.selected_proven(&target.opaque_id, Some(target.expected_revision))
                    .map(|(entry, _, resolved)| (entry.id(), resolved.proof))
            })
            .collect::<BackendResult<Vec<_>>>()?;
        super::ipc::remove_many(&proven)?;
        self.clear_identity_state()?;
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
        opaque_id: &str,
        expected_revision: Option<u64>,
        favorite: bool,
    ) -> BackendResult<OperationResult> {
        let (entry, _, resolved) = self.selected_proven(opaque_id, expected_revision)?;
        super::ipc::favorite(entry.id(), &resolved.proof, favorite)?;
        self.clear_identity_state()?;
        let action = if favorite { "favorite" } else { "unfavorite" };
        Ok(OperationResult::completed(action, "Favorite state updated"))
    }

    pub(super) fn cleanup_artifacts(&self) -> BackendResult<OperationResult> {
        let removed = cleanup_backend(self)?;
        Ok(OperationResult::completed(
            "cleanup",
            &format!("Clipboard caches cleared; {removed} unreferenced generated files removed"),
        ))
    }

    pub(super) fn wipe_entries(&self) -> BackendResult<OperationResult> {
        super::ipc::wipe()?;
        cleanup_backend(self)?;
        self.artifact_registry()?.clear_all()?;
        self.clear_identity_state()?;
        Ok(OperationResult::completed(
            "wipe",
            "Clipboard history cleared",
        ))
    }
}

fn cleanup_backend(backend: &RingboardBackend) -> BackendResult<usize> {
    super::content::clear_cache()?;
    let runtime = runtime_directory("clip-daemon")?;
    fs::remove_dir_all(runtime).map_err(operation_error)?;
    let references = backend.generated_artifact_references()?;
    backend.artifact_registry()?.reconcile(&references)
}

async fn run_annotation(
    backend: RingboardBackend,
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
    // Give the picker time to hide so the editor becomes focused when it maps.
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
    backend: &RingboardBackend,
    opaque_id: &str,
    revision: u64,
    output: &Path,
    max_bytes: u64,
) -> BackendResult<OperationCompletion> {
    if !valid_edited_image(output, max_bytes) {
        return Err(operation_error("Annotation returned an invalid image"));
    }
    let raw_id = backend.replace_file_entry(opaque_id, Some(revision), output, "image/png")?;
    let mut completion = OperationCompletion {
        status: "completed",
        message: "Image edit completed",
        warning: None,
    };
    if let Err(error) = publish_annotation(backend, raw_id, output, max_bytes) {
        completion.message = "Image edit committed";
        completion.warning = Some(format!("Clipboard publication failed: {error}"));
    }
    Ok(completion)
}

fn publish_annotation(
    backend: &RingboardBackend,
    raw_id: u64,
    output: &Path,
    max_bytes: u64,
) -> BackendResult<()> {
    let source = backend.details_raw_sync(raw_id, super::MAX_DETAILS_BYTES)?;
    let bytes = read_path(output, max_bytes)?;
    backend
        .artifact_registry()?
        .register_inline_echo(&source.entry.id, "image/png", &bytes)?;
    backend.clear_identity_state()?;
    backend.selection.publish("image/png", bytes, max_bytes)?;
    backend.artifact_registry()?.clear_active_selection();
    Ok(())
}

fn selected_bytes(
    backend: &RingboardBackend,
    opaque_id: &str,
    expected_revision: Option<u64>,
    max_bytes: u64,
) -> BackendResult<(crate::model::EntrySummary, Vec<u8>)> {
    let (entry, mut reader, summary) = backend.selected(opaque_id, expected_revision)?;
    let bytes = read_entry(entry, &mut reader, max_bytes)?;
    Ok((summary, bytes))
}

fn selected_image(
    backend: &RingboardBackend,
    opaque_id: &str,
    expected_revision: Option<u64>,
    max_bytes: u64,
) -> BackendResult<(crate::model::EntrySummary, Vec<u8>)> {
    let selected = selected_bytes(backend, opaque_id, expected_revision, max_bytes)?;
    (selected.0.kind == EntryKind::Image)
        .then_some(selected)
        .ok_or_else(|| invalid_entry("Only image entries support this action"))
}

fn resolved_content<'a>(mime: &'a str, bytes: &[u8], max_bytes: u64) -> ResolvedContent<'a> {
    ResolvedContent::resolve(mime, bytes, max_bytes.min(MAX_WAYLAND_SELECTION_BYTES))
}

fn publish_entry(
    backend: &RingboardBackend,
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

fn publish_image_uri(backend: &RingboardBackend, path: &Path, max_bytes: u64) -> BackendResult<()> {
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
    fn staging_never_overwrites_or_removes_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("edit");
        super::write_private(&path, b"original").unwrap();
        assert!(super::write_private(&path, b"replacement").is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"original");
    }
}
