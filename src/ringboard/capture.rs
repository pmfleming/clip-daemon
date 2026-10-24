//! Capture uses negotiated policy IPC, never unchecked legacy mutation.
use std::{
    fs::File,
    io::{Read, Seek},
    os::unix::fs::FileExt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use super::{
    RingboardBackend,
    content::{inspect_entry, load_entry, stored_mime_type},
    ipc,
};
use crate::{backend::BackendResult, capture::CaptureSink};

impl CaptureSink for RingboardBackend {
    fn ready(&self, max_bytes: u64) -> Result<(), String> {
        let limits = ipc::capture_ready().map_err(|error| error.to_string())?;
        if limits.max_entry_bytes != Some(max_bytes) {
            return Err("Capture limit does not match the running policy engine".into());
        }
        Ok(())
    }

    fn ingest(&self, mime: &str, file: &File) -> Result<(), String> {
        // Retain the backend's SDK read panic-containment boundary.
        catch_unwind(AssertUnwindSafe(|| ingest_admitted(self, mime, file)))
            .map_err(|_| "Ringboard capture SDK failed; ingestion stopped".to_owned())?
    }
}

fn ingest_admitted(backend: &RingboardBackend, mime: &str, mut file: &File) -> Result<(), String> {
    let _transaction = backend
        .transaction
        .lock()
        .map_err(|_| "History transaction unavailable")?;
    let size = file
        .metadata()
        .map_err(|_| "Could not inspect captured bytes")?
        .len();
    if size > crate::backend::MAX_WAYLAND_SELECTION_BYTES {
        return Ok(());
    }
    file.rewind()
        .map_err(|_| "Could not rewind captured bytes")?;
    let (_, digest) = inspect_entry(&mut file, size, 0).map_err(|error| error.to_string())?;
    let stored_mime = super::storage_mime(mime);
    let proof = ipc::content_proof(&digest, stored_mime);
    let candidate = candidate(file, size, stored_mime).map_err(|error| error.to_string())?;
    file.rewind()
        .map_err(|_| "Could not rewind captured bytes")?;
    if ipc::capture(candidate, &proof, mime, file)
        .map_err(|error| error.to_string())?
        .is_some()
    {
        backend
            .clear_identity_state()
            .map_err(|error| error.to_string())?;
    } else {
        tracing::debug!(
            reason = "engine-admission",
            "capture rejected without persistence"
        );
    }
    Ok(())
}

/// A hint, not a mutation proof: v2 revalidates full content and MIME in one
/// server reactor turn. Match the old watcher's 2048-main/16-favorite window,
/// without its unsafe BorrowedBuf helper, unchecked IDs, or content-only hashes.
fn candidate(file: &File, size: u64, mime: &str) -> BackendResult<Option<u64>> {
    let (database, mut reader) = RingboardBackend::open()?;
    let mut budget = 128 * 1024 * 1024_u64;
    for entry in database
        .favorites()
        .rev()
        .take(16)
        .chain(database.main().rev().take(2048))
    {
        let Ok((mut loaded, metadata)) = load_entry(entry, &mut reader) else {
            continue;
        };
        if metadata.len() != size || stored_mime_type(&loaded)? != mime {
            continue;
        }
        if equal_candidate(&mut *loaded, file, size, &mut budget) {
            return Ok(Some(entry.id()));
        }
        if budget == 0 {
            break;
        }
    }
    Ok(None)
}

/// Bounded comparison uses positional reads on the admitted FD and never maps
/// historical entries into memory. Budget exhaustion safely falls back to Add.
fn equal_candidate(source: &mut impl Read, file: &File, size: u64, budget: &mut u64) -> bool {
    let mut actual = [0; 16 * 1024];
    let mut expected = [0; 16 * 1024];
    let mut offset = 0;
    while offset < size {
        let count = (size - offset).min(actual.len() as u64).min(*budget) as usize;
        if count == 0 {
            return false;
        }
        *budget -= count as u64;
        if source.read_exact(&mut actual[..count]).is_err()
            || file.read_exact_at(&mut expected[..count], offset).is_err()
            || actual[..count] != expected[..count]
        {
            return false;
        }
        offset += count as u64;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::equal_candidate;
    use std::io::{self, Read, Seek, SeekFrom, Write};

    struct ShortReads<'a>(&'a [u8], bool);
    impl Read for ShortReads<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.1) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = output.len().min(7);
            self.0.read(&mut output[..count])
        }
    }

    #[test]
    fn comparisons_respect_budget_content_and_positional_io() {
        let mut bytes = vec![b'a'; 33_000];
        let size = bytes.len() as u64;
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&bytes).unwrap();
        file.seek(SeekFrom::Start(11)).unwrap();
        for (length, mut budget, matches) in [
            (bytes.len(), size, true),
            (bytes.len(), size - 1, false),
            (bytes.len() - 1, size, false),
            (bytes.len(), 0, false),
        ] {
            assert_eq!(
                equal_candidate(
                    &mut ShortReads(&bytes[..length], true),
                    &file,
                    size,
                    &mut budget
                ),
                matches
            );
            assert_eq!(file.stream_position().unwrap(), 11);
            assert_eq!(budget, 0);
        }
        *bytes.last_mut().unwrap() = b'b';
        let mut budget = size;
        assert!(!equal_candidate(&mut &bytes[..], &file, size, &mut budget));
        file.set_len(size - 1).unwrap();
        budget = size;
        assert!(!equal_candidate(&mut &bytes[..], &file, size, &mut budget));
        assert!(equal_candidate(&mut &b""[..], &file, 0, &mut 0));
    }
}
