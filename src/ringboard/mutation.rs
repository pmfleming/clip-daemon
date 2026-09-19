use std::{
    collections::HashMap,
    env,
    fs::{self, File, OpenOptions},
    future::Future,
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command as StdCommand,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use clipboard_history_client_sdk::{Entry, EntryReader};
use image::ImageReader;
use tokio::sync::{broadcast, oneshot};
use url::Url;
use uuid::Uuid;

use crate::{
    backend::{
        BackendError, BackendErrorKind, BackendResult, ClipboardBackend, EntryTarget,
        MAX_WAYLAND_SELECTION_BYTES, ScreenshotRegion,
    },
    model::{EntryKind, OperationResult},
};

use super::{
    MAX_THUMBNAIL_BYTES, OperationControl, OperationTask, RingboardBackend,
    content::{Publication, ResolvedContent, ResolvedImage},
    invalid_entry, run_blocking,
};

const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct AnnotationStage {
    input: PathBuf,
    output: PathBuf,
    opaque_id: String,
    revision: u64,
    max_bytes: u64,
}

struct OperationCompletion {
    message: &'static str,
    warning: Option<String>,
}

impl OperationCompletion {
    fn event(self, id: String, action: &str) -> OperationResult {
        let mut event = OperationResult::with_id(id, action, "completed", self.message);
        event.warning = self.warning;
        event
    }
}

async fn complete_annotation(
    id: String,
    ready: oneshot::Receiver<()>,
    operations: Arc<Mutex<HashMap<String, OperationTask>>>,
    events: broadcast::Sender<OperationResult>,
    control: Arc<OperationControl>,
    run: impl Future<Output = BackendResult<OperationCompletion>>,
) {
    let result = if ready.await.is_ok() {
        run.await
    } else {
        Err(operation_error("Clipboard operation could not start"))
    };
    if !claim_terminal_event(&operations, &id) {
        control.finish();
        return;
    }
    let event = match result {
        Ok(completion) => completion.event(id, "annotate"),
        Err(error) => OperationResult::with_id(id, "annotate", "failed", &error.to_string()),
    };
    let _ = events.send(event);
    control.finish();
}

fn claim_terminal_event(
    operations: &Mutex<HashMap<String, OperationTask>>,
    operation_id: &str,
) -> bool {
    operations
        .lock()
        .is_ok_and(|mut active| active.remove(operation_id).is_some())
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

    pub(super) fn capture_region(
        &self,
        region: ScreenshotRegion,
        max_bytes: u64,
    ) -> BackendResult<OperationResult> {
        let directory = runtime_directory("clip-daemon/screenshots")?;
        let path = unique_path(&directory, "png");
        drop(private_file(&path)?);
        let result = capture_and_publish(self, region, &path, max_bytes);
        let _ = fs::remove_file(path);
        if result.is_ok() {
            self.selection_changed()?;
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
        publish_entry(self, &summary, &bytes, max_bytes)?;
        self.selection_changed()?;
        Ok(completed(
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
        let (path, created) = prepare_image_file(&content, &bytes)?;
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
        let files = vec![staged.input.clone(), staged.output.clone()];
        let mut operation = OperationResult::completed("annotate", "Image editor started");
        operation.status = "started".into();
        let operation_id = operation.id.clone();
        let (start, ready) = oneshot::channel();
        let control = Arc::new(OperationControl::default());
        let handle = tokio::spawn(complete_annotation(
            operation_id.clone(),
            ready,
            Arc::clone(&self.operations),
            self.operation_events.clone(),
            Arc::clone(&control),
            run_annotation(self.clone(), staged, Arc::clone(&control)),
        ));
        let mut active = match self.operations.lock() {
            Ok(active) => active,
            Err(_) => {
                handle.abort();
                remove_files(&files);
                return Err(operation_error("Clipboard operation state is unavailable"));
            }
        };
        active.insert(
            operation_id,
            OperationTask {
                action: "annotate",
                control,
                handle,
                files,
            },
        );
        drop(active);
        let _ = self.operation_events.send(operation.clone());
        start
            .send(())
            .map_err(|_| operation_error("Annotation task could not be started"))?;
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
        let content = resolved_content(&summary.mime, &bytes, limit);
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
        private_file(&path)?
            .write_all(bytes)
            .map_err(operation_error)?;
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
        Ok(completed("delete", "Clipboard entry deleted"))
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
        Ok(completed(
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
        Ok(completed(
            "cleanup",
            &format!("Clipboard caches cleared; {removed} unreferenced generated files removed"),
        ))
    }

    pub(super) async fn stop_operations(&self) -> BackendResult<()> {
        let active: Vec<_> = self
            .operations
            .lock()
            .map_err(|_| super::lock_error())?
            .iter()
            .map(|(id, task)| (id.clone(), Arc::clone(&task.control)))
            .collect();
        for (id, control) in active {
            if !self.cancel_operation(&id).await? {
                // Never hold the backend transaction lock while waiting: the
                // committing job needs it to complete and release staged files.
                control.wait().await;
            }
        }
        Ok(())
    }

    pub(super) fn wipe_entries(&self) -> BackendResult<OperationResult> {
        super::ipc::wipe()?;
        cleanup_backend(self)?;
        self.artifact_registry()?.clear_all()?;
        self.clear_identity_state()?;
        Ok(completed("wipe", "Clipboard history cleared"))
    }
}

fn cleanup_backend(backend: &RingboardBackend) -> BackendResult<usize> {
    super::content::clear_cache()?;
    let runtime = runtime_directory("clip-daemon")?;
    fs::remove_dir_all(runtime).map_err(operation_error)?;
    let references = backend.generated_artifact_references()?;
    backend.artifact_registry()?.reconcile(&references)
}

fn capture_and_publish(
    backend: &RingboardBackend,
    region: ScreenshotRegion,
    path: &Path,
    max_bytes: u64,
) -> BackendResult<OperationResult> {
    let geometry = format!(
        "{},{} {}x{}",
        region.x, region.y, region.width, region.height
    );
    let mut command = StdCommand::new("grim");
    command.args(["-g", &geometry]).arg(path);
    let status = command_status_with_timeout(&mut command, SCREENSHOT_TIMEOUT)?;
    if !status.success() {
        return Err(operation_error("Screenshot capture failed"));
    }
    if !valid_edited_image(path, max_bytes.min(MAX_THUMBNAIL_BYTES)) {
        return Err(operation_error(
            "Screenshot capture returned an invalid image",
        ));
    }
    backend
        .selection
        .publish_file("image/png", path, max_bytes.min(MAX_THUMBNAIL_BYTES))?;
    Ok(completed(
        "screenshot",
        "Screenshot published to the Wayland clipboard",
    ))
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
    match backend
        .editor
        .run(&input, &output)
        .await
        .map_err(operation_error)
    {
        Ok(()) if output.is_file() => {
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
        Ok(()) => Err(operation_error("Image edit was cancelled")),
        Err(error) => Err(error),
    }
}

fn command_status_with_timeout(
    command: &mut StdCommand,
    timeout: Duration,
) -> BackendResult<std::process::ExitStatus> {
    let mut child = command
        .spawn()
        .map_err(|_| operation_error("Could not start screenshot capture"))?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().map_err(operation_error)? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(operation_error("Screenshot capture timed out"))
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
    let bytes = fs::read(output).map_err(operation_error)?;
    backend
        .artifact_registry()?
        .register_inline_echo(&source.entry.id, "image/png", &bytes)?;
    backend.clear_identity_state()?;
    backend
        .selection
        .publish_file("image/png", output, max_bytes)?;
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

fn resolved_content(mime: &str, bytes: &[u8], max_bytes: u64) -> ResolvedContent {
    ResolvedContent::resolve(mime, bytes, max_bytes.min(MAX_WAYLAND_SELECTION_BYTES))
}

fn publish_entry(
    backend: &RingboardBackend,
    summary: &crate::model::EntrySummary,
    bytes: &[u8],
    max_bytes: u64,
) -> BackendResult<()> {
    let content = resolved_content(&summary.mime, bytes, max_bytes);
    if summary.kind == EntryKind::Image && content.kind() != EntryKind::Image {
        return Err(invalid_entry(
            "Clipboard image file is missing, unsafe, invalid, or exceeds the size limit",
        ));
    }
    match content.default_publication() {
        Publication::Bytes { mime } => backend.selection.publish(mime, bytes.to_vec(), max_bytes),
        Publication::File { mime, path } => backend.selection.publish_file(mime, path, max_bytes),
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

fn publish_image_uri(backend: &RingboardBackend, path: &Path, max_bytes: u64) -> BackendResult<()> {
    let uri = Url::from_file_path(path)
        .map_err(|_| operation_error("Could not create image file URI"))?;
    backend
        .selection
        .publish_file_link(format!("{uri}\r\n").into_bytes(), max_bytes)
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
    let limit = max_bytes.min(MAX_WAYLAND_SELECTION_BYTES);
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

pub(super) fn valid_edited_image(path: &Path, max_bytes: u64) -> bool {
    if !path.symlink_metadata().is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() <= max_bytes.min(MAX_THUMBNAIL_BYTES)
    }) {
        return false;
    }
    let Some(mut reader) = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .ok()
    else {
        return false;
    };
    if reader.format() != Some(image::ImageFormat::Png) {
        return false;
    }
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

pub(super) fn runtime_directory(child: &str) -> BackendResult<PathBuf> {
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

pub(super) fn private_file(path: &Path) -> BackendResult<File> {
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

pub(super) fn unique_path(directory: &Path, extension: &str) -> PathBuf {
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

pub(super) fn operation_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{
        OperationTask, claim_terminal_event, command_status_with_timeout, valid_edited_image,
    };

    #[tokio::test]
    async fn only_one_terminal_path_can_claim_an_operation() {
        let operations = std::sync::Mutex::new(std::collections::HashMap::from([(
            "operation-1".to_owned(),
            OperationTask {
                action: "annotate",
                control: Default::default(),
                handle: tokio::spawn(async {}),
                files: Vec::new(),
            },
        )]));

        assert!(claim_terminal_event(&operations, "operation-1"));
        assert!(!claim_terminal_event(&operations, "operation-1"));
    }

    #[tokio::test]
    async fn annotation_cancellation_removes_files_and_emits_one_terminal_event() {
        use crate::backend::ClipboardBackend;
        for start_first in [false, true] {
            let backend = super::RingboardBackend::default();
            let mut events = backend.operation_events.subscribe();
            let directory = tempfile::tempdir().unwrap();
            let staged = directory.path().join("staged.png");
            fs::write(&staged, b"fixture").unwrap();
            let (start, ready) = tokio::sync::oneshot::channel();
            let (started, running) = tokio::sync::oneshot::channel();
            let control = std::sync::Arc::new(super::OperationControl::default());
            let handle = tokio::spawn(super::complete_annotation(
                "cancel-me".into(),
                ready,
                backend.operations.clone(),
                backend.operation_events.clone(),
                control.clone(),
                async move {
                    let _ = started.send(());
                    std::future::pending().await
                },
            ));
            backend.operations.lock().unwrap().insert(
                "cancel-me".into(),
                OperationTask {
                    action: "annotate",
                    control,
                    handle,
                    files: vec![staged.clone()],
                },
            );
            if start_first {
                start.send(()).unwrap();
                running.await.unwrap();
            }
            assert!(backend.cancel_operation("cancel-me").await.unwrap());
            assert!(!backend.cancel_operation("cancel-me").await.unwrap());
            let event = events.try_recv().unwrap();
            assert_eq!(event.status, "cancelled");
            assert_eq!(event.id, "cancel-me");
            assert!(events.try_recv().is_err());
            assert!(!staged.exists());
            assert!(backend.operations.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn cleanup_waits_for_a_blocking_commit_and_keeps_its_real_outcome() {
        use std::sync::Arc;
        let backend = super::RingboardBackend::default();
        let mut events = backend.operation_events.subscribe();
        let directory = tempfile::tempdir().unwrap();
        let staged = directory.path().join("staged.png");
        fs::write(&staged, b"fixture").unwrap();
        let control = Arc::new(super::OperationControl::default());
        assert!(control.begin_commit());
        let (release, blocked) = std::sync::mpsc::channel();
        let (entered, running) = tokio::sync::oneshot::channel();
        let (start, ready) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(super::complete_annotation(
            "committing".into(),
            ready,
            backend.operations.clone(),
            backend.operation_events.clone(),
            control.clone(),
            async move {
                tokio::task::spawn_blocking(move || {
                    let _ = entered.send(());
                    blocked.recv().unwrap();
                })
                .await
                .unwrap();
                Ok(super::OperationCompletion {
                    message: "Committed",
                    warning: None,
                })
            },
        ));
        backend.operations.lock().unwrap().insert(
            "committing".into(),
            OperationTask {
                action: "annotate",
                control,
                handle,
                files: vec![staged.clone()],
            },
        );
        start.send(()).unwrap();
        running.await.unwrap();
        assert!(
            !super::ClipboardBackend::cancel_operation(&backend, "committing")
                .await
                .unwrap()
        );
        assert!(staged.exists());
        assert!(events.try_recv().is_err());
        let other = backend.clone();
        let cleanup = tokio::spawn(async move { other.stop_operations().await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!cleanup.is_finished());
        release.send(()).unwrap();
        cleanup.await.unwrap().unwrap();
        assert_eq!(events.try_recv().unwrap().status, "completed");
        assert!(events.try_recv().is_err());
        assert!(!staged.exists());
    }

    #[test]
    fn screenshot_commands_are_bounded() {
        let mut slow = std::process::Command::new("sleep");
        slow.arg("1");
        let started = std::time::Instant::now();
        assert!(
            command_status_with_timeout(&mut slow, std::time::Duration::from_millis(40)).is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
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
        for format in [
            image::ImageFormat::Jpeg,
            image::ImageFormat::Gif,
            image::ImageFormat::Tiff,
        ] {
            let disguised = directory.path().join("not-really-png.png");
            image::RgbImage::new(2, 2)
                .save_with_format(&disguised, format)
                .unwrap();
            assert!(
                !valid_edited_image(&disguised, super::MAX_THUMBNAIL_BYTES),
                "{format:?}"
            );
        }
        let link = directory.path().join("symlink.png");
        std::os::unix::fs::symlink(&valid, &link).unwrap();
        assert!(!valid_edited_image(&link, super::MAX_THUMBNAIL_BYTES));
    }
}
