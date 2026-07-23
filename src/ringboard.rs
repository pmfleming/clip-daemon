use std::{
    collections::HashMap,
    io::Read,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use clipboard_history_client_sdk::{DatabaseReader, Entry, EntryReader};
use sha2::{Digest, Sha256};

use crate::{
    backend::{ClipboardBackend, HistoryQuery},
    classification::{INSPECTION_LIMIT, bounded_preview, classify},
    model::{BackendStatus, EntryDetails, EntrySummary, HistoryPage},
};

const DEFAULT_MIME: &str = "text/plain";
const MAX_QUERY_LIMIT: usize = 200;

pub struct RingboardBackend {
    ids: Mutex<HashMap<String, u64>>,
    revision: AtomicU64,
}

impl Default for RingboardBackend {
    fn default() -> Self {
        Self {
            ids: Mutex::new(HashMap::new()),
            revision: AtomicU64::new(1),
        }
    }
}

impl RingboardBackend {
    fn open() -> Result<(DatabaseReader, EntryReader)> {
        let mut directory = clipboard_history_client_sdk::core::dirs::data_dir();
        let database = DatabaseReader::open(&mut directory).context("open Ringboard database")?;
        let reader = EntryReader::open(&mut directory).context("open Ringboard entry reader")?;
        Ok((database, reader))
    }

    fn summarize(&self, entry: Entry, reader: &mut EntryReader) -> Result<EntrySummary> {
        let mut loaded = entry.to_file(reader).context("open Ringboard entry")?;
        let byte_size = loaded
            .metadata()
            .context("read Ringboard entry metadata")?
            .len();
        let mime_value = loaded.mime_type().context("read Ringboard MIME metadata")?;
        let mime = if mime_value.is_empty() {
            DEFAULT_MIME
        } else {
            mime_value.as_str()
        };
        let mut bytes = Vec::with_capacity(INSPECTION_LIMIT.min(byte_size as usize));
        (&mut *loaded)
            .take(INSPECTION_LIMIT as u64)
            .read_to_end(&mut bytes)
            .context("read bounded Ringboard preview")?;
        let id = opaque_id(entry.id());
        self.ids
            .lock()
            .map_err(|_| anyhow!("Ringboard ID registry lock poisoned"))?
            .insert(id.clone(), entry.id());
        Ok(EntrySummary {
            revision: entry_revision(entry.id(), byte_size, mime),
            id,
            kind: classify(mime, &bytes),
            mime: mime.to_owned(),
            byte_size,
            favorite: entry.ring()
                == clipboard_history_client_sdk::core::protocol::RingKind::Favorites,
            preview: bounded_preview(&bytes, INSPECTION_LIMIT),
        })
    }

    fn resolve(&self, opaque: &str) -> Result<u64> {
        self.ids
            .lock()
            .map_err(|_| anyhow!("Ringboard ID registry lock poisoned"))?
            .get(opaque)
            .copied()
            .ok_or_else(|| anyhow!("unknown or stale clipboard entry ID"))
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
                detail: format!("{error:#}"),
            },
        }
    }

    async fn query(&self, query: HistoryQuery) -> Result<HistoryPage> {
        let (database, mut reader) = Self::open()?;
        let requested = query.limit.clamp(1, MAX_QUERY_LIMIT);
        let candidates: Vec<_> = database
            .favorites()
            .rev()
            .chain(database.main().rev())
            .collect();
        let mut entries = Vec::with_capacity(requested);
        let needle = query.query.trim().to_lowercase();
        let mut matched = 0usize;
        for entry in candidates {
            let summary = self.summarize(entry, &mut reader)?;
            if !needle.is_empty()
                && !summary.preview.to_lowercase().contains(&needle)
                && !summary.mime.to_lowercase().contains(&needle)
            {
                continue;
            }
            matched += 1;
            if entries.len() < requested {
                entries.push(summary);
            }
        }
        Ok(HistoryPage {
            revision: self.revision.fetch_add(1, Ordering::Relaxed),
            generation: query.generation,
            has_more: matched > entries.len(),
            entries,
        })
    }

    async fn details(&self, opaque_id: &str, max_text_bytes: usize) -> Result<EntryDetails> {
        let raw_id = self.resolve(opaque_id)?;
        let (database, mut reader) = Self::open()?;
        let entry = database
            .get_raw(raw_id)
            .context("resolve Ringboard entry")?;
        let summary = self.summarize(entry, &mut reader)?;
        let mut loaded = entry
            .to_file(&mut reader)
            .context("open Ringboard detail")?;
        let limit = max_text_bytes.min(256 * 1024);
        let mut bytes = Vec::with_capacity(limit.min(summary.byte_size as usize));
        (&mut *loaded)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .context("read bounded Ringboard detail")?;
        let truncated = bytes.len() > limit;
        bytes.truncate(limit);
        let text = std::str::from_utf8(&bytes).ok().map(str::to_owned);
        Ok(EntryDetails {
            entry: summary,
            text,
            preview_truncated: truncated,
        })
    }
}

fn opaque_id(raw_id: u64) -> String {
    let digest = Sha256::digest(
        [
            b"clip-daemon:ringboard:v1:".as_slice(),
            &raw_id.to_le_bytes(),
        ]
        .concat(),
    );
    format!("entry-{}", hex::encode(&digest[..16]))
}

fn entry_revision(raw_id: u64, size: u64, mime: &str) -> u64 {
    let digest = Sha256::digest(
        [
            b"clip-daemon:revision:v1:".as_slice(),
            &raw_id.to_le_bytes(),
            &size.to_le_bytes(),
            mime.as_bytes(),
        ]
        .concat(),
    );
    u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix has eight bytes"),
    )
}

#[cfg(test)]
mod tests {
    use super::{entry_revision, opaque_id};

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
}
