use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
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
    backend::{BackendError, BackendErrorKind, BackendResult},
    model::{EntryKind, OperationResult},
};

use super::{MAX_THUMBNAIL_BYTES, RingboardBackend, invalid_entry};

impl RingboardBackend {
    pub(super) fn restore_entry(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        let (entry, mut reader, _) = self.selected(opaque_id)?;
        send_paste_buffer(paste_server()?, entry, &mut reader, false).map_err(operation_error)?;
        Ok(OperationResult::completed(
            "copy",
            "Entry restored to the clipboard",
        ))
    }

    pub(super) fn save_image_file(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        let (entry, mut reader, summary) = self.selected(opaque_id)?;
        if summary.kind != EntryKind::Image {
            return Err(invalid_entry("Only image entries can be saved as files"));
        }
        let directory = image_directory()?;
        let path = unique_path(&directory, image_extension(&summary.mime));
        copy_entry(entry, &mut reader, &path)?;
        let uri = Url::from_file_path(&path)
            .map_err(|_| operation_error("Could not create image file URI"))?;
        add_and_restore(format!("{uri}\r\n").as_bytes(), "text/uri-list")?;
        let mut result = OperationResult::completed("image-as-file", "Image file copied");
        result.path = Some(path.to_string_lossy().into_owned());
        Ok(result)
    }

    pub(super) fn start_annotation(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        let (input, output, raw_id, ring) = self.stage_annotation(opaque_id)?;
        let mut operation = OperationResult::completed("annotate", "Satty annotation started");
        operation.status = "started".into();
        let task = tokio::spawn(run_annotation(
            input,
            output,
            raw_id,
            ring,
            operation.id.clone(),
        ));
        self.operations
            .lock()
            .map_err(|_| operation_error("Clipboard operation state is unavailable"))?
            .insert(operation.id.clone(), task);
        Ok(operation)
    }

    fn stage_annotation(
        &self,
        opaque_id: &str,
    ) -> BackendResult<(PathBuf, PathBuf, u64, RingKind)> {
        let (entry, mut reader, summary) = self.selected(opaque_id)?;
        if summary.kind != EntryKind::Image {
            return Err(invalid_entry("Only image entries can be annotated"));
        }
        let directory = runtime_directory("clip-daemon/edits")?;
        let input = unique_path(&directory, image_extension(&summary.mime));
        let output = unique_path(&directory, "png");
        copy_entry(entry, &mut reader, &input)?;
        Ok((input, output, entry.id(), target_ring(summary.favorite)))
    }

    pub(super) fn replace_entry(
        &self,
        opaque_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        let (entry, _, summary) = self.selected(opaque_id)?;
        let directory = runtime_directory("clip-daemon/transfers")?;
        let path = unique_path(&directory, "edit");
        private_file(&path)?
            .write_all(bytes)
            .map_err(operation_error)?;
        let result = replace_from_file(entry.id(), target_ring(summary.favorite), &path, mime);
        let _ = fs::remove_file(path);
        result
    }

    pub(super) fn remove_entry(&self, opaque_id: &str) -> BackendResult<OperationResult> {
        let raw_id = self.resolve(opaque_id)?;
        remove_raw(server()?, raw_id)?;
        Ok(OperationResult::completed(
            "delete",
            "Clipboard entry deleted",
        ))
    }

    pub(super) fn move_entry(
        &self,
        opaque_id: &str,
        favorite: bool,
    ) -> BackendResult<OperationResult> {
        let raw_id = self.resolve(opaque_id)?;
        let server = server()?;
        let target = target_ring(favorite);
        let response =
            MoveToFrontRequest::response(server, raw_id, Some(target)).map_err(operation_error)?;
        if matches!(response, MoveToFrontResponse::Error(_)) {
            return Err(operation_error("Ringboard rejected the favorite change"));
        }
        let action = if favorite { "favorite" } else { "unfavorite" };
        Ok(OperationResult::completed(action, "Favorite state updated"))
    }

    pub(super) fn cleanup_artifacts(&self) -> BackendResult<OperationResult> {
        for (_, task) in self
            .operations
            .lock()
            .map_err(|_| operation_error("Clipboard operation state is unavailable"))?
            .drain()
        {
            task.abort();
        }
        super::content::clear_cache()?;
        let runtime = runtime_directory("clip-daemon")?;
        fs::remove_dir_all(&runtime).map_err(operation_error)?;
        Ok(OperationResult::completed(
            "cleanup",
            "Clipboard caches cleared",
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
        Ok(OperationResult::completed(
            "wipe",
            "Clipboard history cleared",
        ))
    }
}

async fn run_annotation(
    input: PathBuf,
    output: PathBuf,
    raw_id: u64,
    ring: RingKind,
    operation_id: String,
) {
    let status = Command::new("satty")
        .args(["--filename", input.to_string_lossy().as_ref()])
        .args(["--output-filename", output.to_string_lossy().as_ref()])
        .args([
            "--resize",
            "smart",
            "--early-exit",
            "--actions-on-enter",
            "save-to-file",
        ])
        .status()
        .await;
    if status.is_ok_and(|value| value.success())
        && valid_edited_image(&output)
        && let Err(error) =
            replace_from_file(raw_id, ring, &output, "image/png").and_then(|()| restore_raw(raw_id))
    {
        tracing::warn!(%operation_id, code = %error.kind.code(), "annotation result could not be restored");
    }
    let _ = fs::remove_file(input);
    let _ = fs::remove_file(output);
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
    path.metadata()
        .is_ok_and(|metadata| metadata.len() <= MAX_THUMBNAIL_BYTES)
        && ImageReader::open(path)
            .and_then(ImageReader::with_guessed_format)
            .ok()
            .and_then(|reader| reader.into_dimensions().ok())
            .is_some()
}

pub(super) fn clear_edit_files() {
    let root = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let _ = fs::remove_dir_all(root.join("clip-daemon/edits"));
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

fn operation_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, error.to_string())
}
