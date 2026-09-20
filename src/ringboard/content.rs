use std::{
    collections::HashSet,
    env,
    fs::{self, File, Permissions},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use clipboard_history_client_sdk::{Entry, EntryReader};
use image::{DynamicImage, ImageReader, Limits};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, MAX_FILES},
    classification::{INSPECTION_LIMIT, classify},
    model::{EntryKind, EntrySummary, EntryThumbnail, FilePreview, ImageMetadata},
    selection::{read_selection, selection_io_error},
};

pub(super) const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;

const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODED_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

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

        let semantic_mime = canonical_mime(&stored_mime);
        let files = match semantic_mime {
            "text/uri-list" | "x-special/gnome-copied-files" | "text/plain" => {
                parse_files(semantic_mime, bytes)
            }
            _ => Vec::new(),
        };
        let local_image = local_image_source_from_files(&files, max_file_bytes);
        let kind = if local_image.is_some() {
            EntryKind::Image
        } else {
            classify(semantic_mime, bytes)
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
            Some(ResolvedImage::Inline { dimensions, .. }) => *dimensions,
            Some(ResolvedImage::LocalFile(source)) => Some(source.dimensions),
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

pub(super) fn read_entry(
    entry: Entry,
    reader: &mut EntryReader,
    max_bytes: u64,
) -> BackendResult<Vec<u8>> {
    let mut source = entry.to_file(reader).map_err(selection_io_error)?;
    let size = source.metadata().map_err(selection_io_error)?.len();
    read_selection(&mut *source, size, max_bytes)
}

pub(super) fn read_path(path: &Path, max_bytes: u64) -> BackendResult<Vec<u8>> {
    let source = File::open(path).map_err(selection_io_error)?;
    let size = source.metadata().map_err(selection_io_error)?.len();
    read_selection(source, size, max_bytes)
}

pub(super) fn read_bounded(file: &mut File, limit: usize) -> BackendResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(INSPECTION_LIMIT));
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_entry("Could not read clipboard entry"))?;
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
    let (width, height) = match cached_dimensions(&path) {
        Some(dimensions) => dimensions,
        None => write_thumbnail(file, &path, edge)?,
    };
    Ok(EntryThumbnail {
        entry_id: summary.id.clone(),
        revision: summary.revision,
        path: path.to_string_lossy().into_owned(),
        width,
        height,
    })
}

fn write_thumbnail(file: &File, path: &Path, edge: u32) -> BackendResult<(u32, u32)> {
    let thumbnail = decode_image(file)?.thumbnail(edge, edge);
    thumbnail
        .save_with_format(path, image::ImageFormat::Png)
        .map_err(|_| invalid_entry("Clipboard thumbnail could not be written"))?;
    private_permissions(path, 0o600)?;
    Ok((thumbnail.width(), thumbnail.height()))
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
    let local_path = url.to_file_path().ok();
    let display_name = local_path
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| {
            url.path_segments()
                .and_then(Iterator::last)
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "File".into());
    Some(FilePreview {
        exists: local_path.is_some_and(|path| path.exists()),
        uri: url.to_string(),
        display_name,
        operation: operation.to_owned(),
    })
}

fn mime_or_default(mime: &str) -> &str {
    if mime.is_empty() { "text/plain" } else { mime }
}

fn canonical_mime(mime: &str) -> &str {
    const ALIASES: [(&str, &str); 6] = [
        ("image/x-png", "image/png"),
        ("image/jpg", "image/jpeg"),
        ("image/pjpeg", "image/jpeg"),
        (
            "application/x-gnome-copied-files",
            "x-special/gnome-copied-files",
        ),
        ("text/x-uri", "text/uri-list"),
        ("text/x-uri-list", "text/uri-list"),
    ];
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    ALIASES
        .iter()
        .find_map(|(alias, canonical)| essence.eq_ignore_ascii_case(alias).then_some(*canonical))
        .unwrap_or(essence)
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
    let path = safe_local_file(&file.uri, max_bytes)?;
    let (mime, dimensions) = inspect_local_image(&path)?;
    Some(LocalImageSource {
        path,
        mime,
        dimensions,
    })
}

fn safe_local_file(uri: &str, max_bytes: u64) -> Option<PathBuf> {
    let path = Url::parse(uri).ok()?.to_file_path().ok()?;
    let metadata = path.symlink_metadata().ok()?;
    (metadata.file_type().is_file() && metadata.len() <= max_bytes).then_some(path)
}

fn inspect_local_image(path: &Path) -> Option<(&'static str, ImageMetadata)> {
    let mut reader = ImageReader::open(path)
        .and_then(ImageReader::with_guessed_format)
        .ok()?;
    let mime = reader.format()?.to_mime_type();
    reader.limits(image_decode_limits());
    let (width, height) = reader.into_dimensions().ok()?;
    Some((mime, ImageMetadata { width, height }))
}

fn image_dimensions(bytes: &[u8]) -> Option<ImageMetadata> {
    let mut reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    reader.limits(image_decode_limits());
    let (width, height) = reader.into_dimensions().ok()?;
    Some(ImageMetadata { width, height })
}

pub(super) fn prune_thumbnails<'a>(valid: impl Iterator<Item = (&'a str, u64)>) {
    let Ok(root) = cache_root() else {
        return;
    };
    let directory = root.join("clip-daemon/thumbnails");
    prune_thumbnail_directory(&directory, valid);
}

fn prune_thumbnail_directory<'a>(directory: &Path, valid: impl Iterator<Item = (&'a str, u64)>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let prefixes: HashSet<String> = valid
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
    use std::os::unix::fs::symlink;

    use super::ResolvedContent;

    #[test]
    fn a_single_local_image_file_can_supply_preview_dimensions() {
        let directory = tempfile::tempdir().expect("image directory");
        let path = directory.path().join("screen shot.png");
        image::RgbaImage::new(7, 5)
            .save(&path)
            .expect("write image fixture");
        let uri = url::Url::from_file_path(&path).unwrap().to_string();
        let inspect =
            |mime, value: &str| ResolvedContent::resolve(mime, value.as_bytes(), 1024 * 1024);
        let uri_list = format!("{uri}\r\n");
        let content = inspect("text/uri-list", &uri_list);
        let source = content.local_image().expect("local image source");
        assert_eq!(source.path, path);
        assert_eq!(source.mime, "image/png");
        assert_eq!((source.dimensions.width, source.dimensions.height), (7, 5));
        assert_eq!(content.kind(), crate::model::EntryKind::Image);
        let plain = inspect("text/plain", &format!("{uri}\n"));
        assert_eq!(plain.kind(), crate::model::EntryKind::Image);
        let multiple = inspect(
            "x-special/gnome-copied-files",
            &format!("cut\n{uri}\n{uri}\n"),
        );
        assert!(multiple.local_image().is_none());
        assert_eq!(multiple.files().len(), 2);
        assert_eq!(multiple.files()[0].operation, "cut");
        assert_eq!(multiple.files()[0].display_name, "screen shot.png");
        assert!(inspect("text/uri-list", "not a uri").files().is_empty());

        let symlink_path = directory.path().join("screenshot-link.png");
        symlink(&path, &symlink_path).expect("image symlink");
        let symlink_uri = url::Url::from_file_path(symlink_path).unwrap().to_string();
        let symlink = inspect("text/uri-list", &format!("{symlink_uri}\r\n"));
        assert!(symlink.local_image().is_none());
    }
}
