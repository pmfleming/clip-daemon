use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command as StdCommand,
};

use clipboard_history_client_sdk::{
    DatabaseReader, EntryReader,
    api::{
        AddRequest, MoveToFrontRequest, RemoveRequest, SwapRequest, connect_to_paste_server,
        connect_to_server, send_paste_buffer,
    },
    core::{
        dirs::{data_dir, paste_socket_file, socket_file},
        protocol::{AddResponse, MimeType, MoveToFrontResponse, RingKind},
    },
};
use image::ImageReader;
use rustix::net::SocketAddrUnix;
use tokio::process::Command;
use url::Url;
use uuid::Uuid;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, ScreenshotRegion},
    model::{EntryKind, OperationResult},
};

use super::{MAX_THUMBNAIL_BYTES, OperationTask, RingboardBackend, invalid_entry, run_blocking};

pub(super) struct AnnotationStage {
    input: PathBuf,
    output: PathBuf,
    opaque_id: String,
    revision: u64,
}

impl RingboardBackend {
    pub(super) fn capture_region(
        &self,
        region: ScreenshotRegion,
    ) -> BackendResult<OperationResult> {
        let directory = runtime_directory("clip-daemon/screenshots")?;
        let path = unique_path(&directory, "png");
        drop(private_file(&path)?);
        let result = capture_and_restore(region, &path);
        let _ = fs::remove_file(path);
        if result.is_ok() {
            self.clear_identity_state()?;
        }
        result
    }

    pub(super) fn restore_entry(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<OperationResult> {
        let paste_server = paste_server()?;
        let (entry, mut reader, _) = self.selected(opaque_id, expected_revision)?;
        send_paste_buffer(paste_server, entry, &mut reader, false).map_err(operation_error)?;
        Ok(completed("copy", "Entry restored to the clipboard"))
    }

    pub(super) fn save_image_file(
        &self,
        opaque_id: &str,
        expected_revision: Option<u64>,
    ) -> BackendResult<OperationResult> {
        let (entry, mut reader, summary) = self.selected(opaque_id, expected_revision)?;
        if summary.kind != EntryKind::Image {
            return Err(invalid_entry("Only image entries can be saved as files"));
        }
        let directory = image_directory()?;
        let path = unique_path(&directory, image_extension(&summary.mime));
        copy_entry(entry, &mut reader, &path)?;
        let uri = Url::from_file_path(&path)
            .map_err(|_| operation_error("Could not create image file URI"))?;
        add_and_restore(format!("{uri}\r\n").as_bytes(), "text/uri-list")?;
        self.clear_identity_state()?;
        let mut result = OperationResult::completed("image-as-file", "Image file copied");
        result.path = Some(path.to_string_lossy().into_owned());
        Ok(result)
    }

    pub(super) fn launch_annotation(
        &self,
        staged: AnnotationStage,
    ) -> BackendResult<OperationResult> {
        let mut operation = OperationResult::completed("annotate", "Satty annotation started");
        operation.status = "started".into();
        let operation_id = operation.id.clone();
        let files = [staged.input.clone(), staged.output.clone()];
        let cleanup_files = files.clone();
        let operations = self.operations.clone();
        let backend = self.clone();
        let task_id = operation_id.clone();
        let (start, ready) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if ready.await.is_ok() {
                run_annotation(backend, staged, &task_id).await;
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
    ) -> BackendResult<AnnotationStage> {
        let (entry, mut reader, summary) = self.selected(opaque_id, expected_revision)?;
        if summary.kind != EntryKind::Image {
            return Err(invalid_entry("Only image entries can be annotated"));
        }
        let directory = runtime_directory("clip-daemon/edits")?;
        let input = unique_path(&directory, image_extension(&summary.mime));
        let output = unique_path(&directory, "png");
        copy_entry(entry, &mut reader, &input)?;
        Ok(AnnotationStage {
            input,
            output,
            opaque_id: opaque_id.to_owned(),
            revision: summary.revision,
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
        Ok(completed("cleanup", "Clipboard caches cleared"))
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
        self.clear_identity_state()?;
        Ok(completed("wipe", "Clipboard history cleared"))
    }
}

fn capture_and_restore(region: ScreenshotRegion, path: &Path) -> BackendResult<OperationResult> {
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
    if !valid_edited_image(path) {
        return Err(operation_error(
            "Screenshot capture returned an invalid image",
        ));
    }
    add_file_and_restore(path, "image/png")?;
    Ok(completed("screenshot", "Clipboard screenshot copied"))
}

async fn run_annotation(backend: RingboardBackend, staged: AnnotationStage, operation_id: &str) {
    let AnnotationStage {
        input,
        output,
        opaque_id,
        revision,
    } = staged;
    if run_satty(&input, &output).await {
        let edited = output.clone();
        let result =
            run_blocking(move || apply_annotation(&backend, &opaque_id, revision, &edited)).await;
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

async fn run_satty(input: &Path, output: &Path) -> bool {
    let mut command = Command::new("satty");
    command.kill_on_drop(true);
    command
        .args(["--filename", input.to_string_lossy().as_ref()])
        .args([
            "--output-filename",
            output.to_string_lossy().as_ref(),
            "--resize",
            "smart",
            "--early-exit",
            "--actions-on-enter",
            "save-to-file",
        ])
        .status()
        .await
        .is_ok_and(|status| status.success())
}

fn apply_annotation(
    backend: &RingboardBackend,
    opaque_id: &str,
    revision: u64,
    output: &Path,
) -> BackendResult<()> {
    if !valid_edited_image(output) {
        return Err(operation_error("Annotation returned an invalid image"));
    }
    let (entry, _, summary) = backend.selected(opaque_id, Some(revision))?;
    replace_from_file(
        entry.id(),
        target_ring(summary.favorite),
        output,
        "image/png",
    )?;
    backend.clear_identity_state()?;
    restore_raw(entry.id())
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

fn restore_raw(raw_id: u64) -> BackendResult<()> {
    let mut directory = data_dir();
    let database = DatabaseReader::open(&mut directory).map_err(operation_error)?;
    let mut reader = EntryReader::open(&mut directory).map_err(operation_error)?;
    let entry = database.get_raw(raw_id).map_err(operation_error)?;
    send_paste_buffer(paste_server()?, entry, &mut reader, false).map_err(operation_error)
}

fn add_and_restore(bytes: &[u8], mime: &str) -> BackendResult<()> {
    let directory = runtime_directory("clip-daemon/transfers")?;
    let path = unique_path(&directory, "data");
    private_file(&path)?
        .write_all(bytes)
        .map_err(operation_error)?;
    let result = add_file_and_restore(&path, mime);
    let _ = fs::remove_file(path);
    result
}

fn add_file_and_restore(path: &Path, mime: &str) -> BackendResult<()> {
    let id = add_file(server()?, path, mime, RingKind::Main)?;
    restore_raw(id)
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

fn paste_server() -> BackendResult<OwnedFd> {
    connect_to_paste_server(&socket_address(paste_socket_file())?).map_err(operation_error)
}

fn socket_address(path: PathBuf) -> BackendResult<SocketAddrUnix> {
    SocketAddrUnix::new(path).map_err(operation_error)
}

fn copy_entry(
    entry: clipboard_history_client_sdk::Entry,
    reader: &mut EntryReader,
    path: &Path,
) -> BackendResult<()> {
    let mut source = entry.to_file(reader).map_err(operation_error)?;
    io::copy(&mut *source, &mut private_file(path)?).map_err(operation_error)?;
    Ok(())
}

fn target_ring(favorite: bool) -> RingKind {
    if favorite {
        RingKind::Favorites
    } else {
        RingKind::Main
    }
}

fn valid_edited_image(path: &Path) -> bool {
    if !path
        .metadata()
        .is_ok_and(|metadata| metadata.len() <= MAX_THUMBNAIL_BYTES)
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

        assert!(valid_edited_image(&valid));
        assert!(!valid_edited_image(&malformed));
    }
}
