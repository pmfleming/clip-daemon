use std::{
    collections::HashSet,
    env,
    fs::{self, File, Permissions},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use image::{DynamicImage, ImageReader, Limits};
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, MAX_WAYLAND_SELECTION_BYTES},
    classification::classify,
    model::{EntryKind, EntrySummary, EntryThumbnail, FilePreview, ImageMetadata},
};

use super::{INSPECTION_LIMIT, MAX_FILES, MAX_THUMBNAIL_BYTES};

const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODED_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

pub(super) struct LocalImageSource {
    pub path: PathBuf,
    pub mime: &'static str,
    pub dimensions: ImageMetadata,
}

pub(super) fn read_bounded(file: &mut File, limit: usize) -> BackendResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(INSPECTION_LIMIT));
    let mut buffer = [0_u8; 8192];
    while bytes.len() < limit {
        let remaining = limit - bytes.len();
        let chunk_size = remaining.min(buffer.len());
        let read = file
            .read(&mut buffer[..chunk_size])
            .map_err(|_| invalid_entry("Could not read clipboard entry"))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

pub(super) fn detected_image_mime(bytes: &[u8]) -> Option<&'static str> {
    image::guess_format(bytes)
        .ok()
        .map(|format| format.to_mime_type())
}

pub(super) fn semantic_kind(mime: &str, bytes: &[u8]) -> EntryKind {
    local_image_source(mime, bytes, MAX_WAYLAND_SELECTION_BYTES)
        .map(|_| EntryKind::Image)
        .unwrap_or_else(|| classify(mime, bytes))
}

pub(super) fn detail_facts(
    summary: &EntrySummary,
    bytes: &[u8],
) -> (Vec<FilePreview>, Option<ImageMetadata>) {
    if accepts_local_image_mime(&summary.mime) {
        let files = parse_files(&summary.mime, bytes);
        if let Some(source) = local_image_source_from_files(&files, MAX_THUMBNAIL_BYTES) {
            return (files, Some(source.dimensions));
        }
        if is_file_list_mime(&summary.mime) {
            return (files, None);
        }
    }
    match summary.kind {
        EntryKind::Image => (Vec::new(), image_dimensions(bytes)),
        _ => (Vec::new(), None),
    }
}

pub(super) fn create_thumbnail(
    file: &File,
    summary: &EntrySummary,
    edge: u32,
) -> BackendResult<EntryThumbnail> {
    if summary.kind != EntryKind::Image || summary.byte_size > MAX_THUMBNAIL_BYTES {
        return Err(invalid_entry("Clipboard entry cannot be thumbnailed"));
    }
    create_thumbnail_from_image(file, summary, edge)
}

pub(super) fn create_file_thumbnail(
    file: &mut File,
    summary: &EntrySummary,
    edge: u32,
) -> BackendResult<EntryThumbnail> {
    let bytes = read_bounded(file, INSPECTION_LIMIT)?;
    let source = local_image_source(&summary.mime, &bytes, MAX_THUMBNAIL_BYTES)
        .ok_or_else(|| invalid_entry("Clipboard file is not a local image"))?;
    let image_file = File::open(&source.path)
        .map_err(|_| invalid_entry("Clipboard image file could not be opened"))?;
    create_thumbnail_from_image(&image_file, summary, edge)
}

fn create_thumbnail_from_image(
    file: &File,
    summary: &EntrySummary,
    edge: u32,
) -> BackendResult<EntryThumbnail> {
    let edge = edge.clamp(32, 1024);
    let path =
        thumbnail_directory()?.join(format!("{}-{}-{edge}.png", summary.id, summary.revision));
    if let Some((width, height)) = cached_dimensions(&path) {
        return Ok(EntryThumbnail {
            entry_id: summary.id.clone(),
            revision: summary.revision,
            path: path.to_string_lossy().into_owned(),
            width,
            height,
        });
    }
    let image = decode_image(file)?;
    let thumbnail = image.thumbnail(edge, edge);
    thumbnail
        .save_with_format(&path, image::ImageFormat::Png)
        .map_err(|_| invalid_entry("Clipboard thumbnail could not be written"))?;
    private_permissions(&path, 0o600)?;
    Ok(EntryThumbnail {
        entry_id: summary.id.clone(),
        revision: summary.revision,
        path: path.to_string_lossy().into_owned(),
        width: thumbnail.width(),
        height: thumbnail.height(),
    })
}

fn cached_dimensions(path: &Path) -> Option<(u32, u32)> {
    ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .ok()?
        .into_dimensions()
        .ok()
}

fn decode_image(file: &File) -> BackendResult<DynamicImage> {
    let mut reader = ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| invalid_entry("Clipboard image format is invalid"))?;
    reader.limits(image_decode_limits());
    reader
        .decode()
        .map_err(|_| invalid_entry("Clipboard image could not be decoded"))
}

pub(super) fn image_decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_IMAGE_BYTES);
    limits
}

fn parse_files(mime: &str, bytes: &[u8]) -> Vec<FilePreview> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut lines = text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let operation = gnome_operation(mime, &mut lines);
    lines
        .take(MAX_FILES)
        .filter_map(|uri| file_preview(uri, operation))
        .collect()
}

fn gnome_operation<'a>(mime: &str, lines: &mut impl Iterator<Item = &'a str>) -> &'a str {
    if mime != "x-special/gnome-copied-files" {
        return "copy";
    }
    lines
        .next()
        .filter(|value| matches!(*value, "copy" | "cut"))
        .unwrap_or("copy")
}

fn file_preview(uri: &str, operation: &str) -> Option<FilePreview> {
    let url = Url::parse(uri).ok()?;
    let display_name = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|name| !name.is_empty())
        .unwrap_or("File")
        .to_owned();
    Some(FilePreview {
        exists: url.to_file_path().ok().is_some_and(|path| path.exists()),
        uri: url.to_string(),
        display_name,
        operation: operation.to_owned(),
    })
}

fn is_file_list_mime(mime: &str) -> bool {
    matches!(
        normalized_mime(mime),
        "text/uri-list" | "x-special/gnome-copied-files"
    )
}

fn accepts_local_image_mime(mime: &str) -> bool {
    is_file_list_mime(mime) || normalized_mime(mime) == "text/plain"
}

pub(super) fn is_inline_image_mime(mime: &str) -> bool {
    normalized_mime(mime).starts_with("image/")
}

fn normalized_mime(mime: &str) -> &str {
    mime.split(';').next().unwrap_or(mime).trim()
}

pub(super) fn local_image_source(
    mime: &str,
    bytes: &[u8],
    max_bytes: u64,
) -> Option<LocalImageSource> {
    accepts_local_image_mime(mime)
        .then(|| parse_files(mime, bytes))
        .and_then(|files| local_image_source_from_files(&files, max_bytes))
}

fn local_image_source_from_files(
    files: &[FilePreview],
    max_bytes: u64,
) -> Option<LocalImageSource> {
    let [file] = files else { return None };
    let path = Url::parse(&file.uri).ok()?.to_file_path().ok()?;
    let metadata = path.symlink_metadata().ok()?;
    (metadata.file_type().is_file() && metadata.len() <= max_bytes).then_some(())?;
    let mut reader = ImageReader::open(&path)
        .and_then(ImageReader::with_guessed_format)
        .ok()?;
    let format = reader.format()?;
    reader.limits(image_decode_limits());
    let (width, height) = reader.into_dimensions().ok()?;
    Some(LocalImageSource {
        path,
        mime: format.to_mime_type(),
        dimensions: ImageMetadata { width, height },
    })
}

fn image_dimensions(bytes: &[u8]) -> Option<ImageMetadata> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(image_decode_limits());
    let (width, height) = reader.into_dimensions().ok()?;
    Some(ImageMetadata { width, height })
}

pub(super) fn prune_thumbnails(valid: &[(String, u64)]) {
    let Ok(root) = cache_root() else {
        return;
    };
    let directory = root.join("clip-daemon/thumbnails");
    prune_thumbnail_directory(&directory, valid);
}

fn prune_thumbnail_directory(directory: &Path, valid: &[(String, u64)]) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let prefixes: HashSet<String> = valid
        .iter()
        .map(|(id, revision)| format!("{id}-{revision}-"))
        .collect();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub(super) fn clear_cache() -> BackendResult<()> {
    let directory = cache_root()?.join("clip-daemon");
    match fs::remove_dir_all(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(BackendError::unavailable(
            "Clipboard cache could not be cleared",
        )),
    }
}

fn thumbnail_directory() -> BackendResult<PathBuf> {
    let directory = cache_root()?.join("clip-daemon/thumbnails");
    fs::create_dir_all(&directory)
        .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))?;
    private_permissions(&directory, 0o700)?;
    Ok(directory)
}

fn cache_root() -> BackendResult<PathBuf> {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| BackendError::unavailable("Clipboard cache directory is unavailable"))
}

fn private_permissions(path: &Path, mode: u32) -> BackendResult<()> {
    fs::set_permissions(path, Permissions::from_mode(mode))
        .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))
}

pub(super) fn invalid_entry(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::{
        fs::File,
        io::{Seek, SeekFrom, Write},
        os::unix::fs::symlink,
    };

    use super::{
        MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION, detected_image_mime, file_preview,
        image_decode_limits, local_image_source, parse_files, prune_thumbnail_directory,
        read_bounded, semantic_kind,
    };

    #[test]
    fn bounded_reads_stop_at_the_requested_limit() {
        let mut file = tempfile::tempfile().expect("temporary file");
        let content = vec![0x5a; 24_000];
        file.write_all(&content).expect("write fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind fixture");

        let bytes = read_bounded(&mut file, 10_000).expect("bounded read");

        assert_eq!(bytes, &content[..10_000]);
    }

    #[test]
    fn image_mime_is_detected_without_loading_ringboard_metadata() {
        let png_header = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
        assert_eq!(detected_image_mime(png_header), Some("image/png"));
        assert_eq!(detected_image_mime(b"ordinary text"), None);
    }

    #[test]
    fn file_metadata_is_parsed_inside_the_daemon() {
        let files = parse_files(
            "x-special/gnome-copied-files",
            b"cut\nfile:///tmp/one.txt\nfile:///tmp/two.txt\n",
        );
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].display_name, "one.txt");
        assert_eq!(files[0].operation, "cut");
        assert_eq!(file_preview("not a uri", "copy"), None);
    }

    #[test]
    fn a_single_local_image_file_can_supply_preview_dimensions() {
        let directory = tempfile::tempdir().expect("image directory");
        let path = directory.path().join("screenshot.png");
        image::RgbaImage::new(7, 5)
            .save(&path)
            .expect("write image fixture");
        let uri = url::Url::from_file_path(&path)
            .expect("file URL")
            .to_string();
        let inspect = |mime, value: &str| local_image_source(mime, value.as_bytes(), 1024 * 1024);
        let uri_list = format!("{uri}\r\n");
        let source = inspect("text/uri-list", &uri_list).expect("local image source");

        assert_eq!(source.path, path);
        assert_eq!(source.mime, "image/png");
        assert_eq!((source.dimensions.width, source.dimensions.height), (7, 5));
        assert_eq!(
            semantic_kind("text/uri-list", uri_list.as_bytes()),
            crate::model::EntryKind::Image
        );
        assert_eq!(
            semantic_kind("text/plain", format!("{uri}\n").as_bytes()),
            crate::model::EntryKind::Image
        );
        assert!(inspect("text/uri-list", &format!("{uri}\r\n{uri}\r\n")).is_none());

        let symlink_path = directory.path().join("screenshot-link.png");
        symlink(&path, &symlink_path).expect("image symlink");
        let symlink_uri = url::Url::from_file_path(symlink_path)
            .expect("symlink URL")
            .to_string();
        assert!(inspect("text/uri-list", &format!("{symlink_uri}\r\n")).is_none());
    }

    #[test]
    fn thumbnail_pruning_keeps_only_current_entry_revisions() {
        let directory = tempfile::tempdir().expect("thumbnail directory");
        let current = directory.path().join("entry-current-7-256.png");
        let stale_revision = directory.path().join("entry-current-6-256.png");
        let removed_entry = directory.path().join("entry-removed-1-256.png");
        for path in [&current, &stale_revision, &removed_entry] {
            File::create(path).expect("thumbnail fixture");
        }

        prune_thumbnail_directory(directory.path(), &[("entry-current".into(), 7)]);

        assert!(current.exists());
        assert!(!stale_revision.exists());
        assert!(!removed_entry.exists());
    }

    #[test]
    fn thumbnail_decode_limits_dimensions_and_allocations() {
        let limits = image_decode_limits();
        assert!(limits.check_dimensions(MAX_IMAGE_DIMENSION + 1, 1).is_err());
        let mut limits = image_decode_limits();
        assert!(limits.reserve(MAX_DECODED_IMAGE_BYTES + 1).is_err());
    }
}
