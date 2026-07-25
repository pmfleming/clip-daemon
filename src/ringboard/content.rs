use std::{
    collections::HashSet,
    env,
    fs::{self, File, Permissions},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use image::{DynamicImage, ImageReader, Limits};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult},
    classification::classify,
    model::{EntryKind, EntrySummary, EntryThumbnail, FilePreview, ImageMetadata},
};

use super::{INSPECTION_LIMIT, MAX_FILES, MAX_THUMBNAIL_BYTES};

const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODED_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct LocalImageSource {
    pub path: PathBuf,
    pub mime: &'static str,
    pub dimensions: ImageMetadata,
}

/// Daemon-owned semantic interpretation of bytes and MIME stored by Ringboard.
pub(super) struct ResolvedContent {
    stored_mime: String,
    kind: EntryKind,
    files: Vec<FilePreview>,
    image: Option<ResolvedImage>,
}

pub(super) enum ResolvedImage {
    Inline {
        mime: &'static str,
        dimensions: Option<ImageMetadata>,
    },
    LocalFile(LocalImageSource),
}

pub(super) enum Publication<'a> {
    Bytes { mime: &'a str },
    File { mime: &'a str, path: &'a Path },
}

impl ResolvedContent {
    pub fn resolve(stored_mime: &str, bytes: &[u8], max_file_bytes: u64) -> Self {
        let stored_mime = mime_or_default(stored_mime).to_owned();
        if let Some(mime) = detected_image_mime(bytes) {
            return Self {
                stored_mime,
                kind: EntryKind::Image,
                files: Vec::new(),
                image: Some(ResolvedImage::Inline {
                    mime,
                    dimensions: image_dimensions(bytes),
                }),
            };
        }

        let semantic_mime = canonical_mime(&stored_mime).to_owned();
        let files = if accepts_local_image_mime(&semantic_mime) {
            parse_files(&semantic_mime, bytes)
        } else {
            Vec::new()
        };
        let local_image = local_image_source_from_files(&files, max_file_bytes);
        let kind = if local_image.is_some() {
            EntryKind::Image
        } else {
            classify(&semantic_mime, bytes)
        };
        Self {
            stored_mime,
            kind,
            files,
            image: local_image.map(ResolvedImage::LocalFile),
        }
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Sniffed inline images use their actual MIME. Other entries preserve the
    /// exact MIME Ringboard captured for publication compatibility.
    pub fn mime(&self) -> &str {
        match &self.image {
            Some(ResolvedImage::Inline { mime, .. }) => mime,
            _ => &self.stored_mime,
        }
    }

    pub fn files(&self) -> &[FilePreview] {
        &self.files
    }

    pub fn image_metadata(&self) -> Option<ImageMetadata> {
        match &self.image {
            Some(ResolvedImage::Inline { dimensions, .. }) => dimensions.clone(),
            Some(ResolvedImage::LocalFile(source)) => Some(source.dimensions.clone()),
            None => None,
        }
    }

    pub fn image(&self) -> Option<&ResolvedImage> {
        self.image.as_ref()
    }

    pub fn local_image(&self) -> Option<&LocalImageSource> {
        match &self.image {
            Some(ResolvedImage::LocalFile(source)) => Some(source),
            _ => None,
        }
    }

    pub fn default_publication(&self) -> Publication<'_> {
        match &self.image {
            Some(ResolvedImage::LocalFile(source)) => Publication::File {
                mime: source.mime,
                path: &source.path,
            },
            _ => Publication::Bytes { mime: self.mime() },
        }
    }
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

pub(super) fn create_resolved_thumbnail(
    stored_file: &File,
    content: &ResolvedContent,
    summary: &EntrySummary,
    edge: u32,
) -> BackendResult<EntryThumbnail> {
    if summary.kind != EntryKind::Image {
        return Err(invalid_entry("Clipboard entry cannot be thumbnailed"));
    }
    let image_file = match content.image() {
        Some(ResolvedImage::Inline { .. }) if summary.byte_size <= MAX_THUMBNAIL_BYTES => {
            stored_file
                .try_clone()
                .map_err(|_| invalid_entry("Could not open clipboard image"))?
        }
        Some(ResolvedImage::LocalFile(source)) => File::open(&source.path)
            .map_err(|_| invalid_entry("Clipboard image file could not be opened"))?,
        _ => return Err(invalid_entry("Clipboard entry cannot be thumbnailed")),
    };
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
        canonical_mime(mime),
        "text/uri-list" | "x-special/gnome-copied-files"
    )
}

fn accepts_local_image_mime(mime: &str) -> bool {
    is_file_list_mime(mime) || canonical_mime(mime) == "text/plain"
}

fn mime_or_default(mime: &str) -> &str {
    if mime.is_empty() { "text/plain" } else { mime }
}

fn canonical_mime(mime: &str) -> &str {
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    if essence.eq_ignore_ascii_case("image/x-png") {
        "image/png"
    } else if essence.eq_ignore_ascii_case("image/jpg")
        || essence.eq_ignore_ascii_case("image/pjpeg")
    {
        "image/jpeg"
    } else if essence.eq_ignore_ascii_case("application/x-gnome-copied-files") {
        "x-special/gnome-copied-files"
    } else if essence.eq_ignore_ascii_case("text/x-uri")
        || essence.eq_ignore_ascii_case("text/x-uri-list")
    {
        "text/uri-list"
    } else {
        essence
    }
}

pub(super) fn image_identity(mime: &str, bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:resolved-image:v1:");
    hasher.update(canonical_mime(mime).as_bytes());
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
    hex::encode(hasher.finalize())
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
        MAX_DECODED_IMAGE_BYTES, MAX_IMAGE_DIMENSION, ResolvedContent, detected_image_mime,
        file_preview, image_decode_limits, parse_files, prune_thumbnail_directory, read_bounded,
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
        let inspect =
            |mime, value: &str| ResolvedContent::resolve(mime, value.as_bytes(), 1024 * 1024);
        let uri_list = format!("{uri}\r\n");
        let content = inspect("text/uri-list", &uri_list);
        let source = content.local_image().expect("local image source");

        assert_eq!(source.path, path);
        assert_eq!(source.mime, "image/png");
        assert_eq!((source.dimensions.width, source.dimensions.height), (7, 5));
        assert_eq!(content.kind(), crate::model::EntryKind::Image);
        assert_eq!(
            inspect("text/plain", &format!("{uri}\n")).kind(),
            crate::model::EntryKind::Image
        );
        assert!(
            inspect("text/uri-list", &format!("{uri}\r\n{uri}\r\n"))
                .local_image()
                .is_none()
        );

        let symlink_path = directory.path().join("screenshot-link.png");
        symlink(&path, &symlink_path).expect("image symlink");
        let symlink_uri = url::Url::from_file_path(symlink_path)
            .expect("symlink URL")
            .to_string();
        assert!(
            inspect("text/uri-list", &format!("{symlink_uri}\r\n"))
                .local_image()
                .is_none()
        );
    }

    #[test]
    fn mime_aliases_share_semantic_policy_without_changing_stored_mime() {
        let content = ResolvedContent::resolve("image/x-png", b"not an image", 1024);
        assert_eq!(content.mime(), "image/x-png");
        assert_eq!(content.kind(), crate::model::EntryKind::Image);
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
