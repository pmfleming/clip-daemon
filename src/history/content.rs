//! Bounded image decoding and semantic file/image interpretation, independent of storage.
use super::{HistoryBackend, MAX_DETAILS_BYTES};
use crate::{
    backend::{
        BackendError, BackendResult, MAX_FILES, MAX_WAYLAND_SELECTION_BYTES,
        invalid_data as invalid_entry,
    },
    classification::classify,
    history_store::StoredEntry,
    model::{EntryDetails, EntryKind, EntryThumbnail, FilePreview, ImageMetadata},
    selection::read_path,
};
use image::{DynamicImage, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, File, Permissions},
    io::{self, BufRead, BufReader, Seek},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};
use url::Url;
pub(crate) const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 16_384;
const MAX_DECODED_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

impl HistoryBackend {
    pub(super) fn thumbnail_sync(
        &self,
        id: &str,
        expected: Option<u64>,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        let stored = self.selected(id, expected)?;
        let content = ResolvedContent::resolve(&stored.mime, &stored.bytes, MAX_THUMBNAIL_BYTES);
        let edge = edge.clamp(32, 1024);
        let path = thumbnail_directory()?.join(format!(
            "{}-{}-{edge}.png",
            stored.summary.id, stored.summary.revision
        ));
        let (width, height) = match cached_dimensions(&path) {
            Some(dimensions) => dimensions,
            None => {
                let image = match content.image() {
                    Some(ResolvedImage::Inline { .. })
                        if stored.summary.byte_size <= MAX_THUMBNAIL_BYTES =>
                    {
                        decode_image(io::Cursor::new(&stored.bytes))?
                    }
                    Some(ResolvedImage::LocalFile(source)) => {
                        let bytes = read_path(&source.path, MAX_THUMBNAIL_BYTES)?;
                        decode_image(io::Cursor::new(bytes))?
                    }
                    _ => return Err(invalid_entry("Clipboard entry cannot be thumbnailed")),
                };
                let thumbnail = image.thumbnail(edge, edge);
                thumbnail
                    .save_with_format(&path, image::ImageFormat::Png)
                    .map_err(|_| invalid_entry("Clipboard thumbnail could not be written"))?;
                private_permissions(&path, 0o600)?;
                (thumbnail.width(), thumbnail.height())
            }
        };
        Ok(EntryThumbnail {
            entry_id: stored.summary.id,
            revision: stored.summary.revision,
            path: path.to_string_lossy().into_owned(),
            width,
            height,
        })
    }
}

pub(super) fn entry_details(
    stored: StoredEntry,
    max_text_bytes: usize,
) -> BackendResult<EntryDetails> {
    let limit = max_text_bytes.min(MAX_DETAILS_BYTES);
    let truncated = stored.bytes.len() > limit;
    let bytes = &stored.bytes[..stored.bytes.len().min(limit)];
    let text = std::str::from_utf8(bytes).ok().map(str::to_owned);
    let content = ResolvedContent::resolve(&stored.mime, bytes, MAX_THUMBNAIL_BYTES);
    Ok(EntryDetails {
        text,
        image: content.image_metadata(),
        files: content.into_files(),
        preview_truncated: truncated,
        entry: stored.summary,
    })
}

pub(crate) struct LocalImageSource {
    pub path: PathBuf,
    pub mime: &'static str,
    pub dimensions: ImageMetadata,
}
impl LocalImageSource {
    pub fn identity(&self) -> Option<String> {
        if !self.path.symlink_metadata().ok()?.is_file() {
            return None;
        }
        let bytes = read_path(&self.path, MAX_WAYLAND_SELECTION_BYTES).ok()?;
        Some(image_identity(self.mime, &bytes))
    }
}
pub(crate) struct ResolvedContent<'a> {
    stored_mime: &'a str,
    kind: EntryKind,
    files: Vec<FilePreview>,
    image: Option<ResolvedImage>,
}
pub(crate) enum ResolvedImage {
    Inline {
        mime: &'static str,
        dimensions: Option<ImageMetadata>,
    },
    LocalFile(LocalImageSource),
}
impl<'a> ResolvedContent<'a> {
    pub fn resolve(stored_mime: &'a str, bytes: &[u8], max_file_bytes: u64) -> Self {
        let stored_mime = if stored_mime.is_empty() {
            "text/plain"
        } else {
            stored_mime
        };
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
        let semantic_mime = canonical_mime(stored_mime);
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
    pub fn mime(&self) -> &str {
        match &self.image {
            Some(ResolvedImage::Inline { mime, .. }) => mime,
            Some(ResolvedImage::LocalFile(source)) => source.mime,
            None => self.stored_mime,
        }
    }
    fn into_files(mut self) -> Vec<FilePreview> {
        self.files.truncate(MAX_FILES);
        self.files
    }
    fn image_metadata(&self) -> Option<ImageMetadata> {
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
}

pub(crate) fn detected_image_mime(bytes: &[u8]) -> Option<&'static str> {
    image::guess_format(bytes)
        .ok()
        .map(|format| format.to_mime_type())
}
fn cached_dimensions(path: &Path) -> Option<(u32, u32)> {
    ImageReader::open(path)
        .and_then(image_reader)
        .ok()?
        .into_dimensions()
        .ok()
}
fn decode_image<R: BufRead + Seek>(source: R) -> BackendResult<DynamicImage> {
    image_reader(ImageReader::new(source))
        .map_err(|_| invalid_entry("Clipboard image format is invalid"))?
        .decode()
        .map_err(|_| invalid_entry("Clipboard image could not be decoded"))
}
/// Edited output must be a regular, bounded PNG that fully decodes.
pub(super) fn read_edited_image(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    if !path.symlink_metadata().is_ok_and(|metadata| {
        metadata.is_file() && metadata.len() <= max_bytes.min(MAX_THUMBNAIL_BYTES)
    }) {
        return None;
    }
    let bytes = read_path(path, max_bytes.min(MAX_THUMBNAIL_BYTES)).ok()?;
    let reader = image_reader(ImageReader::new(io::Cursor::new(&bytes))).ok()?;
    (reader.format() == Some(image::ImageFormat::Png) && reader.decode().is_ok()).then_some(bytes)
}
fn image_reader<R: BufRead + Seek>(reader: ImageReader<R>) -> io::Result<ImageReader<R>> {
    let mut reader = reader.with_guessed_format()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_IMAGE_BYTES);
    reader.limits(limits);
    Ok(reader)
}
fn parse_files(mime: &str, bytes: &[u8]) -> Vec<FilePreview> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut lines = text
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let operation = if mime == "x-special/gnome-copied-files" {
        lines
            .next()
            .filter(|value| matches!(*value, "copy" | "cut"))
            .unwrap_or("copy")
    } else {
        "copy"
    };
    // Inspect enough valid URIs to distinguish a single-image selection from
    // a longer file list, even when invalid lines precede later references.
    lines
        .filter_map(|uri| file_preview(uri, operation))
        .take(MAX_FILES + 1)
        .collect()
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
pub(crate) fn image_identity(mime: &str, bytes: &[u8]) -> String {
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
    if !metadata.is_file() || metadata.len() > max_bytes {
        return None;
    }
    let file = File::open(&path).ok()?;
    let reader = image_reader(ImageReader::new(BufReader::new(file))).ok()?;
    let mime = reader.format()?.to_mime_type();
    let (width, height) = reader.into_dimensions().ok()?;
    Some(LocalImageSource {
        path,
        mime,
        dimensions: ImageMetadata { width, height },
    })
}
fn image_dimensions(bytes: &[u8]) -> Option<ImageMetadata> {
    let (width, height) = image_reader(ImageReader::new(io::Cursor::new(bytes)))
        .ok()?
        .into_dimensions()
        .ok()?;
    Some(ImageMetadata { width, height })
}
pub(super) fn clear_cache() -> BackendResult<()> {
    match fs::remove_dir_all(cache_root()?.join("clip-daemon")) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
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
    prune_thumbnail_cache(&directory);
    Ok(directory)
}
/// Bound disposable thumbnails without loading history IDs or deleting URI targets.
fn prune_thumbnail_cache(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut cached: Vec<_> = entries
        .take(1024)
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name
                .to_str()?
                .strip_prefix("entry-")?
                .strip_suffix(".png")?;
            let id = name.get(..36)?;
            uuid::Uuid::parse_str(id).ok()?;
            let (revision, edge) = name.get(37..)?.split_once('-')?;
            revision.parse::<u64>().ok()?;
            edge.parse::<u32>().ok()?;
            let metadata = entry.path().symlink_metadata().ok()?;
            metadata
                .is_file()
                .then(|| (metadata.modified().ok(), entry.path()))
        })
        .collect();
    cached.sort_by_key(|(modified, _)| *modified);
    let excess = cached.len().saturating_sub(255);
    for (_, path) in cached.into_iter().take(excess) {
        let _ = fs::remove_file(path);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edited_images_must_fully_decode() {
        use image::ImageFormat::{Gif, Jpeg, Png, Tiff};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        for (format, width, valid) in [
            (Jpeg, 2, false),
            (Gif, 2, false),
            (Tiff, 2, false),
            (Png, 16_385, false),
            (Png, 2, true),
        ] {
            image::RgbImage::new(width, 2)
                .save_with_format(&path, format)
                .unwrap();
            let bytes = read_edited_image(&path, MAX_THUMBNAIL_BYTES);
            assert_eq!(bytes.is_some(), valid);
            if let Some(bytes) = bytes {
                assert_eq!(bytes, fs::read(&path).unwrap());
                fs::write(&path, b"changed after validation").unwrap();
                assert!(image::load_from_memory(&bytes).is_ok());
                fs::write(&path, &bytes).unwrap();
            }
        }
        assert!(read_edited_image(&path, 1).is_none());
        let link = directory.path().join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(read_edited_image(&link, MAX_THUMBNAIL_BYTES).is_none());
        fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        assert!(read_edited_image(&path, MAX_THUMBNAIL_BYTES).is_none());
    }
    #[test]
    fn thumbnail_cache_pruning_is_bounded_and_name_scoped() {
        let directory = tempfile::tempdir().unwrap();
        let unrelated = directory.path().join("private.png");
        fs::write(&unrelated, b"private").unwrap();
        for _ in 0..270 {
            fs::write(
                directory
                    .path()
                    .join(format!("entry-{}-1-64.png", uuid::Uuid::new_v4())),
                b"thumbnail",
            )
            .unwrap();
        }
        prune_thumbnail_cache(directory.path());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 256);
        assert_eq!(fs::read(unrelated).unwrap(), b"private");
    }

    #[test]
    fn semantic_file_images_and_mime_authority() {
        for (mime, bytes, kind) in [
            ("image/png", &b"https://example.test"[..], EntryKind::Image),
            ("application/octet-stream", b"hello", EntryKind::Binary),
            ("text/plain", b"https://example.test", EntryKind::Link),
            ("text/plain", br#"{"ok":true}"#, EntryKind::Json),
            ("text/plain", b"#1a2b3c", EntryKind::Color),
            ("", b"ordinary text", EntryKind::Text),
            ("", &[0xff, 0xfe], EntryKind::Binary),
        ] {
            assert_eq!(ResolvedContent::resolve(mime, bytes, 1024).kind(), kind);
        }
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("screen shot.png");
        image::RgbaImage::new(7, 5).save(&path).unwrap();
        let uri = Url::from_file_path(&path).unwrap().to_string();
        for mime in ["text/plain", "text/uri-list", "text/x-uri"] {
            let content = ResolvedContent::resolve(mime, uri.as_bytes(), 1024 * 1024);
            assert_eq!(content.kind(), EntryKind::Image);
            assert_eq!(content.local_image().unwrap().path, path);
        }
        let content = ResolvedContent::resolve(
            "x-special/gnome-copied-files",
            format!("cut\n{uri}\n{uri}\n").as_bytes(),
            1024 * 1024,
        );
        assert!(content.local_image().is_none());
        let files = content.into_files();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].operation, "cut");
        let link = directory.path().join("link.png");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        let link = Url::from_file_path(link).unwrap();
        assert!(
            ResolvedContent::resolve("text/uri-list", link.as_str().as_bytes(), 1024 * 1024)
                .local_image()
                .is_none()
        );
    }
}
