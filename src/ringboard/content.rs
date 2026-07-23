use std::{
    env,
    fs::{self, File, Permissions},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

use image::{DynamicImage, ImageReader};
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult},
    model::{EntryKind, EntrySummary, EntryThumbnail, FilePreview, ImageMetadata},
};

use super::{INSPECTION_LIMIT, MAX_FILES, MAX_THUMBNAIL_BYTES};

pub(super) fn read_bounded(file: &mut File, limit: usize) -> BackendResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(INSPECTION_LIMIT));
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_entry("Could not read clipboard entry"))?;
    Ok(bytes)
}

pub(super) fn detail_facts(
    summary: &EntrySummary,
    bytes: &[u8],
) -> (Vec<FilePreview>, Option<ImageMetadata>) {
    match summary.kind {
        EntryKind::Files => (parse_files(&summary.mime, bytes), None),
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
    let image = decode_image(file)?;
    let thumbnail = image.thumbnail(edge.clamp(32, 1024), edge.clamp(32, 1024));
    let path = thumbnail_directory()?.join(format!("{}-{}.png", summary.id, summary.revision));
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

fn decode_image(file: &File) -> BackendResult<DynamicImage> {
    ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .map_err(|_| invalid_entry("Clipboard image format is invalid"))?
        .decode()
        .map_err(|_| invalid_entry("Clipboard image could not be decoded"))
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

fn image_dimensions(bytes: &[u8]) -> Option<ImageMetadata> {
    let reader = ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (width, height) = reader.into_dimensions().ok()?;
    Some(ImageMetadata { width, height })
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

fn private_permissions(path: &std::path::Path, mode: u32) -> BackendResult<()> {
    fs::set_permissions(path, Permissions::from_mode(mode))
        .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))
}

pub(super) fn invalid_entry(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::{file_preview, parse_files};

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
}
