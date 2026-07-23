use std::{
    collections::HashMap,
    env,
    fs::{self, File, Permissions},
    io::{BufReader, Read},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::Mutex,
};

use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader};
use image::ImageReader;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendResult, ClipboardBackend, HistoryQuery},
    classification::{INSPECTION_LIMIT, bounded_preview, classify},
    model::{
        BackendStatus, EntryDetails, EntryKind, EntrySummary, EntryThumbnail, FilePreview,
        HistoryPage, ImageMetadata,
    },
};

const DEFAULT_MIME: &str = "text/plain";
const MAX_QUERY_LIMIT: usize = 200;
const MAX_DETAILS_BYTES: usize = 256 * 1024;
const MAX_THUMBNAIL_BYTES: u64 = 32 * 1024 * 1024;
const MAX_FILES: usize = 100;

#[derive(Default)]
struct RevisionState {
    token: Option<u64>,
    revision: u64,
}

pub struct RingboardBackend {
    ids: Mutex<HashMap<String, u64>>,
    revision: Mutex<RevisionState>,
}

impl Default for RingboardBackend {
    fn default() -> Self {
        Self {
            ids: Mutex::new(HashMap::new()),
            revision: Mutex::new(RevisionState::default()),
        }
    }
}

impl RingboardBackend {
    fn open() -> BackendResult<(DatabaseReader, EntryReader)> {
        let mut directory = clipboard_history_client_sdk::core::dirs::data_dir();
        let database = DatabaseReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard history is unavailable"))?;
        let reader = EntryReader::open(&mut directory)
            .map_err(|_| BackendError::unavailable("Ringboard entries are unavailable"))?;
        Ok((database, reader))
    }

    fn open_entry(&self, opaque_id: &str) -> BackendResult<(Entry, EntryReader)> {
        let raw_id = self.resolve(opaque_id)?;
        let (database, reader) = Self::open()?;
        let entry = database
            .get_raw(raw_id)
            .map_err(|_| BackendError::not_found("Clipboard entry is stale or missing"))?;
        Ok((entry, reader))
    }

    fn summarize(
        &self,
        entry: Entry,
        reader: &mut EntryReader,
        current: bool,
    ) -> BackendResult<EntrySummary> {
        let mut loaded = entry
            .to_file(reader)
            .map_err(|_| invalid_entry("Could not open clipboard entry"))?;
        let byte_size = loaded
            .metadata()
            .map_err(|_| invalid_entry("Could not read clipboard entry metadata"))?
            .len();
        let mime_value = loaded
            .mime_type()
            .map_err(|_| invalid_entry("Could not read clipboard MIME metadata"))?;
        let mime = if mime_value.is_empty() {
            DEFAULT_MIME
        } else {
            mime_value.as_str()
        };
        let bytes = read_bounded(&mut loaded, INSPECTION_LIMIT)?;
        let id = opaque_id(entry.id());
        self.ids
            .lock()
            .map_err(|_| lock_error())?
            .insert(id.clone(), entry.id());
        Ok(EntrySummary {
            revision: entry_revision(entry.id(), byte_size, mime),
            id,
            kind: classify(mime, &bytes),
            mime: mime.to_owned(),
            byte_size,
            favorite: entry.ring()
                == clipboard_history_client_sdk::core::protocol::RingKind::Favorites,
            current,
            preview: bounded_preview(&bytes, INSPECTION_LIMIT),
        })
    }

    fn revision_for(&self, token: u64) -> BackendResult<u64> {
        let mut state = self.revision.lock().map_err(|_| lock_error())?;
        if state.token != Some(token) {
            state.token = Some(token);
            state.revision = state.revision.saturating_add(1).max(1);
        }
        Ok(state.revision)
    }

    fn resolve(&self, opaque: &str) -> BackendResult<u64> {
        self.ids
            .lock()
            .map_err(|_| lock_error())?
            .get(opaque)
            .copied()
            .ok_or_else(|| BackendError::not_found("Clipboard entry ID is unknown or stale"))
    }

    fn detail_facts(
        summary: &EntrySummary,
        bytes: &[u8],
    ) -> (Vec<FilePreview>, Option<ImageMetadata>) {
        match summary.kind {
            EntryKind::Files => (parse_files(&summary.mime, bytes), None),
            EntryKind::Image => (Vec::new(), image_dimensions(bytes)),
            _ => (Vec::new(), None),
        }
    }
}

#[async_trait]
impl ClipboardBackend for RingboardBackend {
    async fn status(&self) -> BackendStatus {
        match Self::open() {
            Ok(_) => BackendStatus {
                available: true,
                engine: "ringboard".into(),
                detail: "database-readable".into(),
            },
            Err(error) => BackendStatus {
                available: false,
                engine: "ringboard".into(),
                detail: error.to_string(),
            },
        }
    }

    async fn change_token(&self) -> BackendResult<u64> {
        let (database, _) = Self::open()?;
        let main = database.main();
        let favorites = database.favorites();
        Ok(u64::from(main.ring().write_head())
            | (u64::from(favorites.ring().write_head()) << 32)
                ^ (main.ring().len() as u64).rotate_left(17)
                ^ (favorites.ring().len() as u64).rotate_left(49))
    }

    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        let (database, mut reader) = Self::open()?;
        let main: Vec<_> = database.main().rev().collect();
        let favorites: Vec<_> = database.favorites().rev().collect();
        let token = history_token(&database);
        let current_raw = main.first().map(Entry::id);
        let requested = query.limit.clamp(1, MAX_QUERY_LIMIT);
        let needle = query.query.trim().to_lowercase();
        let mut entries = Vec::with_capacity(requested);
        let mut current = None;
        let mut matched = 0;

        for entry in favorites.into_iter().chain(main) {
            let is_current = current_raw == Some(entry.id());
            let summary = self.summarize(entry, &mut reader, is_current)?;
            if is_current {
                current = Some(summary.clone());
            }
            if matches_query(&summary, &needle) {
                matched += 1;
                if entries.len() < requested {
                    entries.push(summary);
                }
            }
        }

        Ok(HistoryPage {
            revision: self.revision_for(token)?,
            generation: query.generation,
            current,
            has_more: matched > entries.len(),
            entries,
        })
    }

    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> BackendResult<EntryDetails> {
        let (entry, mut reader) = self.open_entry(opaque_id)?;
        let summary = self.summarize(entry, &mut reader, false)?;
        let mut loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard details"))?;
        let limit = max_text_bytes.min(MAX_DETAILS_BYTES);
        let mut bytes = read_bounded(&mut loaded, limit.saturating_add(1))?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
        let (files, image) = Self::detail_facts(&summary, &bytes);
        Ok(EntryDetails {
            entry: summary,
            text,
            files,
            image,
            preview_truncated: truncated,
        })
    }

    async fn thumbnail(&self, opaque_id: &str, edge: u32) -> BackendResult<EntryThumbnail> {
        let (entry, mut reader) = self.open_entry(opaque_id)?;
        let summary = self.summarize(entry, &mut reader, false)?;
        if summary.kind != EntryKind::Image || summary.byte_size > MAX_THUMBNAIL_BYTES {
            return Err(invalid_entry("Clipboard entry cannot be thumbnailed"));
        }
        let loaded = entry
            .to_file(&mut reader)
            .map_err(|_| invalid_entry("Could not open clipboard image"))?;
        let image = ImageReader::new(BufReader::new(&*loaded))
            .with_guessed_format()
            .map_err(|_| invalid_entry("Clipboard image format is invalid"))?
            .decode()
            .map_err(|_| invalid_entry("Clipboard image could not be decoded"))?;
        let thumbnail = image.thumbnail(edge.clamp(32, 1024), edge.clamp(32, 1024));
        let directory = thumbnail_directory()?;
        let path = directory.join(format!("{}-{}.png", summary.id, summary.revision));
        thumbnail
            .save_with_format(&path, image::ImageFormat::Png)
            .map_err(|_| invalid_entry("Clipboard thumbnail could not be written"))?;
        fs::set_permissions(&path, Permissions::from_mode(0o600))
            .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))?;
        Ok(EntryThumbnail {
            entry_id: summary.id,
            revision: summary.revision,
            path: path.to_string_lossy().into_owned(),
            width: thumbnail.width(),
            height: thumbnail.height(),
        })
    }
}

fn history_token(database: &DatabaseReader) -> u64 {
    let main = database.main();
    let favorites = database.favorites();
    u64::from(main.ring().write_head())
        | (u64::from(favorites.ring().write_head()) << 32)
            ^ (main.ring().len() as u64).rotate_left(17)
            ^ (favorites.ring().len() as u64).rotate_left(49)
}

fn matches_query(summary: &EntrySummary, needle: &str) -> bool {
    needle.is_empty()
        || summary.preview.to_lowercase().contains(needle)
        || summary.mime.to_lowercase().contains(needle)
}

fn read_bounded(file: &mut File, limit: usize) -> BackendResult<Vec<u8>> {
    let mut bytes = Vec::with_capacity(limit.min(INSPECTION_LIMIT));
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid_entry("Could not read clipboard entry"))?;
    Ok(bytes)
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
    lines
        .take(MAX_FILES)
        .filter_map(|uri| file_preview(uri, operation))
        .collect()
}

fn file_preview(uri: &str, operation: &str) -> Option<FilePreview> {
    let url = Url::parse(uri).ok()?;
    let display_name = url
        .path_segments()
        .and_then(Iterator::last)
        .filter(|name| !name.is_empty())
        .unwrap_or("File")
        .to_owned();
    let exists = url.to_file_path().ok().is_some_and(|path| path.exists());
    Some(FilePreview {
        display_name,
        uri: url.to_string(),
        exists,
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

fn thumbnail_directory() -> BackendResult<PathBuf> {
    let root = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| BackendError::unavailable("Clipboard cache directory is unavailable"))?;
    let directory = root.join("clip-daemon/thumbnails");
    fs::create_dir_all(&directory)
        .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))?;
    fs::set_permissions(&directory, Permissions::from_mode(0o700))
        .map_err(|_| BackendError::unavailable("Clipboard thumbnail cache is unavailable"))?;
    Ok(directory)
}

fn invalid_entry(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}

fn lock_error() -> BackendError {
    BackendError::new(
        BackendErrorKind::OperationFailed,
        "Clipboard backend state is unavailable",
    )
}

fn opaque_id(raw_id: u64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:ringboard:v1:");
    hasher.update(raw_id.to_le_bytes());
    let digest = hasher.finalize();
    format!("entry-{}", hex::encode(&digest[..16]))
}

fn entry_revision(raw_id: u64, size: u64, mime: &str) -> u64 {
    let mut hasher = Sha256::new();
    hasher.update(b"clip-daemon:revision:v1:");
    hasher.update(raw_id.to_le_bytes());
    hasher.update(size.to_le_bytes());
    hasher.update(mime.as_bytes());
    u64::from_le_bytes(
        hasher.finalize()[..8]
            .try_into()
            .expect("fixed SHA-256 prefix"),
    )
}

#[cfg(test)]
mod tests {
    use super::{entry_revision, file_preview, opaque_id, parse_files};

    #[test]
    fn engine_ids_are_not_exposed_and_revisions_are_stable() {
        assert!(opaque_id(42).starts_with("entry-"));
        assert!(!opaque_id(42).contains("42"));
        assert_eq!(opaque_id(42), opaque_id(42));
        assert_eq!(
            entry_revision(1, 2, "text/plain"),
            entry_revision(1, 2, "text/plain")
        );
        assert_ne!(
            entry_revision(1, 2, "text/plain"),
            entry_revision(1, 3, "text/plain")
        );
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
}
