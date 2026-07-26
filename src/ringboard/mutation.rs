use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command as StdCommand,
    time::Duration,
};

use clipboard_history_client_sdk::{
    Entry, EntryReader,
    api::{AddRequest, MoveToFrontRequest, RemoveRequest, SwapRequest, connect_to_server},
    core::{
        dirs::socket_file,
        protocol::{AddResponse, MimeType, MoveToFrontResponse, RingKind},
    },
};
use image::ImageReader;
use rustix::net::SocketAddrUnix;
use url::Url;
use uuid::Uuid;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, ScreenshotRegion},
    editor::ImageEditorCommand,
    model::{EntryKind, OperationResult},
    selection::{SelectionService, effective_limit},
};

use super::{
    MAX_THUMBNAIL_BYTES, OperationTask, RingboardBackend,
    content::{Publication, ResolvedContent, ResolvedImage},
    invalid_entry, run_blocking,
};

pub(super) struct AnnotationStage {
    input: PathBuf,
    output: PathBuf,
    opaque_id: String,
    revision: u64,
    max_bytes: u64,
}

impl RingboardBackend {
    pub(super) fn capture_region(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let directory = runtime_directory("clip-daemon/screenshots")?;
        let path = unique_path(&directory, "png");
        drop(private_file(&path)?);
        let result = capture_and_publish(region, &path, &self.selection, max_bytes);
        let _ = fs::remove_file(path);
        if result.is_ok() {
            self.artifacts
                .lock()
                .map_err(|_| operation_error("Generated-file registry is unavailable"))?
                .clear_active_selection();
            self.clear_identity_state()?;
        }
        result
    }

    pub(super) fn restore_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_bytes(self, opaque_id, expected_revision, max_bytes)?;
        publish_entry(&self.selection, &summary, &bytes, max_bytes)?;
        self.artifacts
            .lock()
            .map_err(|_| operation_error("Generated-file registry is unavailable"))?
            .clear_active_selection();
        self.clear_identity_state()?;
        Ok(completed(
            "copy",
            "Entry published to the Wayland clipboard",
        ))
    }

    pub(super) fn save_image_file(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let (summary, bytes) = selected_image(self, opaque_id, expected_revision, max_bytes)?;
        let content = ResolvedContent::resolve(&summary.mime, &bytes, effective_limit(max_bytes));
        let (path, created) = prepare_image_file(&content, &bytes)?;
        if created {
            if let Err(error) = self
                .artifacts
                .lock()
                .map_err(|_| operation_error("Generated-file registry is unavailable"))?
                .register(&path, &summary.id, content.mime(), &bytes)
            {
                let _ = fs::remove_file(&path);
                return Err(error);
            }
        } else {
            self.artifacts
                .lock()
                .map_err(|_| operation_error("Generated-file registry is unavailable"))?
                .activate_if_generated(&path);
        }
        if let Err(error) = publish_image_uri(&self.selection, &path, max_bytes) {
            if created {
                self.artifacts
                    .lock()
                    .map_err(|_| operation_error("Generated-file registry is unavailable"))?
                    .forget(&path);
                let _ = fs::remove_file(&path);
            }
            return Err(error);
        }
        self.clear_identity_state()?;
        let message = if created {
            "Image file copied"
        } else {
            "Image file link copied"
        };
        let mut result = OperationResult::completed("image-as-file", message);
        result.path = Some(path.to_string_lossy().into_owned());
        Ok(result)
    }

    pub(super) fn launch_annotation(
        &self,
        staged: AnnotationStage,
    ) -> BackendResult<OperationResult> {
        let mut operation = OperationResult::completed("annotate", "Image editor started");
        operation.status = "started".into();
        let operation_id = operation.id.clone();
        let files = [staged.input.clone(), staged.output.clone()];
        let cleanup_files = files.clone();
        let operations = self.operations.clone();
        let backend = self.clone();
        let editor = self.editor.clone();
        let task_id = operation_id.clone();
        let (start, ready) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if ready.await.is_ok() {
                run_annotation(backend, editor, staged, &task_id).await;
            }
            if let Ok(mut active) = operations.lock() {
                active.remove(&task_id);
            }
        });
        let mut active = match self.operations.lock() {
            Ok(active) => active,
            Err(_) => {
                handle.abort();
                remove_files(&cleanup_files);
                return Err(operation_error("Clipboard operation state is unavailable"));
            }
        };
        active.insert(operation_id, OperationTask { handle, files });
        drop(active);
        if start.send(()).is_err() {
            remove_files(&cleanup_files);
            return Err(operation_error("Annotation task could not be started"));
        }
        Ok(operation)
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
        let content = ResolvedContent::resolve(&summary.mime, &bytes, effective_limit(limit));
        let (mime, image_bytes) = match content.image() {
            Some(ResolvedImage::Inline { mime, .. }) => ((*mime).to_owned(), bytes),
            Some(ResolvedImage::LocalFile(source)) => {
                (source.mime.to_owned(), read_path(&source.path, limit)?)
            }
            None => return Err(invalid_entry("Only image entries support this action")),
        };
        let directory = runtime_directory("clip-daemon/edits")?;
        let input = unique_path(&directory, image_extension(&mime));
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

    pub(super) fn replace_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<u64> {
        let directory = runtime_directory("clip-daemon/transfers")?;
        let path = unique_path(&directory, "edit");
        private_file(&path)?
            .write_all(bytes)
            .map_err(operation_error)?;
        let result = self
            .selected(opaque_id, expected_revision)
            .and_then(|(entry, _, summary)| {
                replace_from_file(entry.id(), target_ring(summary.favorite), &path, mime)?;
                Ok(entry.id())
            });
        let _ = fs::remove_file(path);
        let raw_id = result?;
        self.clear_identity_state()?;
        Ok(raw_id)
    }

    pub(super) fn remove_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<OperationResult> {
        let server = server()?;
        let (entry, _, _) = self.selected(opaque_id, expected_revision)?;
        remove_raw(server, entry.id())?;
        self.clear_identity_state()?;
        Ok(completed("delete", "Clipboard entry deleted"))
    }

    pub(super) fn move_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
        favorite: bool,
    ) -> BackendResult<OperationResult> {
        let server = server()?;
        let (entry, _, _) = self.selected(opaque_id, expected_revision)?;
        let target = target_ring(favorite);
        let response = MoveToFrontRequest::response(server, entry.id(), Some(target))
            .map_err(operation_error)?;
        if matches!(response, MoveToFrontResponse::Error(_)) {
            return Err(operation_error("Ringboard rejected the favorite change"));
        }
        self.clear_identity_state()?;
        let action = if favorite { "favorite" } else { "unfavorite" };
        Ok(OperationResult::completed(action, "Favorite state updated"))
    }

    pub(super) fn cleanup_artifacts(&self) -> BackendResult<OperationResult> {
        for (_, operation) in self
            .operations
            .lock()
            .map_err(|_| operation_error("Clipboard operation state is unavailable"))?
            .drain()
        {
            operation.handle.abort();
        }
        super::content::clear_cache()?;
        let runtime = runtime_directory("clip-daemon")?;
        fs::remove_dir_all(&runtime).map_err(operation_error)?;
        let references = self.generated_artifact_references()?;
        let removed = self
            .artifacts
            .lock()
            .map_err(|_| operation_error("Generated-file registry is unavailable"))?
            .reconcile(&references)?;
        Ok(completed(
            "cleanup",
            &format!("Clipboard caches cleared; {removed} unreferenced generated files removed"),
        ))
    }

    pub(super) fn wipe_entries(&self) -> BackendResult<OperationResult> {
        let (database, _) = Self::open()?;
        let ids: Vec<_> = database
            .favorites()
            .chain(database.main())
            .map(|entry| entry.id())
            .collect();
        let server = server()?;
        for id in ids {
            remove_raw(&server, id)?;
        }
        self.cleanup_artifacts()?;
        self.artifacts
            .lock()
            .map_err(|_| operation_error("Generated-file registry is unavailable"))?
            .clear_all()?;
        self.clear_identity_state()?;
        Ok(completed("wipe", "Clipboard history cleared"))
    }
}

fn capture_and_publish(
    region: ScreenshotRegion,
    path: &Path,
    selection: &SelectionService,
    max_bytes: u64,
) -> BackendResult<OperationResult> {
    let geometry = format!(
        "{},{} {}x{}",
        region.x, region.y, region.width, region.height
    );
    let status = StdCommand::new("grim")
        .args(["-g", &geometry])
        .arg(path)
        .status()
        .map_err(|_| operation_error("Could not start screenshot capture"))?;
    if !status.success() {
        return Err(operation_error("Screenshot capture failed"));
    }
    if !valid_edited_image(path, max_bytes.min(MAX_THUMBNAIL_BYTES)) {
        return Err(operation_error(
            "Screenshot capture returned an invalid image",
        ));
    }
    selection.publish_file("image/png", path, max_bytes.min(MAX_THUMBNAIL_BYTES))?;
    Ok(completed(
        "screenshot",
        "Screenshot published to the Wayland clipboard",
    ))
}

async fn run_annotation(
    backend: RingboardBackend,
    editor: ImageEditorCommand,
    staged: AnnotationStage,
    operation_id: &str,
) {
    let AnnotationStage {
        input,
        output,
        opaque_id,
        revision,
        max_bytes,
    } = staged;
    // Give the picker time to hide so the editor becomes focused when it maps.
    tokio::time::sleep(Duration::from_millis(150)).await;
    if run_editor(&editor, &input, &output).await && output.is_file() {
        let edited = output.clone();
        let result = run_blocking(move || {
            apply_annotation(&backend, &opaque_id, revision, &edited, max_bytes)
        })
        .await;
        if let Err(error) = result {
            tracing::warn!(%operation_id, code = %error.kind.code(), "annotation result could not be restored");
        }
    }
    let _ = run_blocking(move || {
        remove_files(&[input, output]);
        Ok(())
    })
    .await;
}

async fn run_editor(editor: &ImageEditorCommand, input: &Path, output: &Path) -> bool {
    editor
        .command(input, output)
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn apply_annotation(
    backend: &RingboardBackend,
    opaque_id: &str,
    revision: u64,
    output: &Path,
    max_bytes: u64,
) -> BackendResult<()> {
    if !valid_edited_image(output, max_bytes) {
        return Err(operation_error("Annotation returned an invalid image"));
    }
    let (entry, _, summary) = backend.selected(opaque_id, Some(revision))?;
    let raw_id = entry.id();
    replace_from_file(raw_id, target_ring(summary.favorite), output, "image/png")?;
    backend.clear_identity_state()?;
    let source = backend.details_raw_sync(raw_id, super::MAX_DETAILS_BYTES)?;
    let bytes = fs::read(output).map_err(operation_error)?;
    backend
        .artifacts
        .lock()
        .map_err(|_| operation_error("Generated-file registry is unavailable"))?
        .register_inline_echo(&source.entry.id, "image/png", &bytes)?;
    backend.clear_identity_state()?;
    backend
        .selection
        .publish_file("image/png", output, max_bytes)?;
    backend
        .artifacts
        .lock()
        .map_err(|_| operation_error("Generated-file registry is unavailable"))?
        .clear_active_selection();
    Ok(())
}

fn replace_from_file(raw_id: u64, ring: RingKind, path: &Path, mime: &str) -> BackendResult<()> {
    let server = server()?;
    let replacement = add_file(&server, path, mime, ring)?;
    let swap = SwapRequest::response(&server, raw_id, replacement).map_err(operation_error)?;
    if swap.error1.is_some() || swap.error2.is_some() {
        return Err(operation_error("Ringboard rejected the replacement swap"));
    }
    remove_raw(server, replacement)
}

fn add_file(server: impl AsFd, path: &Path, mime: &str, ring: RingKind) -> BackendResult<u64> {
    let file = File::open(path).map_err(operation_error)?;
    let mime = MimeType::from(mime).map_err(operation_error)?;
    let AddResponse::Success { id } =
        AddRequest::response_add_unchecked(server, ring, &mime, file).map_err(operation_error)?;
    Ok(id)
}

fn remove_raw(server: impl AsFd, id: u64) -> BackendResult<()> {
    let response = RemoveRequest::response(server, id).map_err(operation_error)?;
    response
        .error
        .is_none()
        .then_some(())
        .ok_or_else(|| operation_error("Ringboard rejected removal"))
}

fn server() -> BackendResult<OwnedFd> {
    connect_to_server(&socket_address(socket_file())?).map_err(operation_error)
}

fn socket_address(path: PathBuf) -> BackendResult<SocketAddrUnix> {
    SocketAddrUnix::new(path).map_err(operation_error)
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

fn publish_entry(
    selection: &SelectionService,
    summary: &crate::model::EntrySummary,
    bytes: &[u8],
    max_bytes: u64,
) -> BackendResult<()> {
    let content = ResolvedContent::resolve(&summary.mime, bytes, effective_limit(max_bytes));
    if summary.kind == EntryKind::Image && content.kind() != EntryKind::Image {
        return Err(invalid_entry(
            "Clipboard image file is missing, unsafe, invalid, or exceeds the size limit",
        ));
    }
    match content.default_publication() {
        Publication::Bytes { mime } => selection.publish(mime, bytes.to_vec(), max_bytes),
        Publication::File { mime, path } => selection.publish_file(mime, path, max_bytes),
    }
}

fn prepare_image_file(content: &ResolvedContent, bytes: &[u8]) -> BackendResult<(PathBuf, bool)> {
    match content.image() {
        Some(ResolvedImage::LocalFile(source)) => Ok((source.path.clone(), false)),
        Some(ResolvedImage::Inline { mime, .. }) => {
            let path = unique_path(&image_directory()?, image_extension(mime));
            write_private(&path, bytes)?;
            Ok((path, true))
        }
        None => Err(invalid_entry("Only image entries support this action")),
    }
}

fn publish_image_uri(
    selection: &SelectionService,
    path: &Path,
    max_bytes: u64,
) -> BackendResult<()> {
    let uri = Url::from_file_path(path)
        .map_err(|_| operation_error("Could not create image file URI"))?;
    selection.publish_file_link(format!("{uri}\r\n").into_bytes(), max_bytes)
}

fn read_entry(entry: Entry, reader: &mut EntryReader, max_bytes: u64) -> BackendResult<Vec<u8>> {
    let mut source = entry.to_file(reader).map_err(operation_error)?;
    let size = source.metadata().map_err(operation_error)?.len();
    read_source(&mut *source, size, max_bytes)
}

fn read_path(path: &Path, max_bytes: u64) -> BackendResult<Vec<u8>> {
    let mut source = File::open(path).map_err(operation_error)?;
    let size = source.metadata().map_err(operation_error)?.len();
    read_source(&mut source, size, max_bytes)
}

fn read_source(source: impl Read, size: u64, max_bytes: u64) -> BackendResult<Vec<u8>> {
    let limit = effective_limit(max_bytes);
    if size > limit {
        return Err(selection_size_error(size, limit));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    source
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(operation_error)?;
    if bytes.len() as u64 > limit {
        return Err(selection_size_error(bytes.len() as u64, limit));
    }
    Ok(bytes)
}

fn selection_size_error(size: u64, limit: u64) -> BackendError {
    BackendError::new(
        BackendErrorKind::InvalidData,
        format!("Clipboard entry is {size} bytes; Wayland publishing is limited to {limit} bytes"),
    )
}

fn target_ring(favorite: bool) -> RingKind {
    if favorite {
        RingKind::Favorites
    } else {
        RingKind::Main
    }
}

fn valid_edited_image(path: &Path, max_bytes: u64) -> bool {
    if !path
        .metadata()
        .is_ok_and(|metadata| metadata.len() <= max_bytes.min(MAX_THUMBNAIL_BYTES))
    {
        return false;
    }
    let Some(mut reader) = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .ok()
    else {
        return false;
    };
    reader.limits(super::content::image_decode_limits());
    reader.decode().is_ok()
}

pub(super) fn remove_files(paths: &[impl AsRef<Path>]) {
    for path in paths {
        let _ = fs::remove_file(path.as_ref());
    }
}

fn image_directory() -> BackendResult<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| operation_error("HOME is unavailable"))?;
    private_directory(PathBuf::from(home).join("Pictures/Screenshots/clipboard-history"))
}

fn runtime_directory(child: &str) -> BackendResult<PathBuf> {
    let root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    private_directory(root.join(child))
}

fn private_directory(path: PathBuf) -> BackendResult<PathBuf> {
    fs::create_dir_all(&path).map_err(operation_error)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(operation_error)?;
    Ok(path)
}

fn private_file(path: &Path) -> BackendResult<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(operation_error)
}

fn write_private(path: &Path, bytes: &[u8]) -> BackendResult<()> {
    let result =
        private_file(path).and_then(|mut file| file.write_all(bytes).map_err(operation_error));
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn unique_path(directory: &Path, extension: &str) -> PathBuf {
    directory.join(format!("clipboard-{}.{}", Uuid::new_v4(), extension))
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

fn completed(action: &str, message: &str) -> OperationResult {
    OperationResult::completed(action, message)
}

fn operation_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{remove_files, valid_edited_image};

    #[test]
    fn operation_cleanup_only_removes_its_own_files() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let unrelated = directory.path().join("unrelated");
        for path in [&first, &second, &unrelated] {
            fs::write(path, b"fixture").expect("write fixture");
        }

        remove_files(&[&first, &second]);

        assert!(!first.exists());
        assert!(!second.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn edited_images_must_fully_decode() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let valid = directory.path().join("valid.png");
        let malformed = directory.path().join("malformed.png");
        image::RgbaImage::new(2, 2)
            .save(&valid)
            .expect("write valid image");
        fs::write(&malformed, b"\x89PNG\r\n\x1a\n").expect("write malformed image");

        assert!(valid_edited_image(&valid, super::MAX_THUMBNAIL_BYTES));
        assert!(!valid_edited_image(&malformed, super::MAX_THUMBNAIL_BYTES));
    }
}
