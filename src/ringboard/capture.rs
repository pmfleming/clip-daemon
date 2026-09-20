//! Capture uses negotiated policy IPC, never unchecked legacy mutation.
use std::{
    fs::File,
    io::{Read, Seek},
    os::unix::fs::FileExt,
    panic::{AssertUnwindSafe, catch_unwind},
};

use sha2::Digest;

use super::{RingboardBackend, ipc, load_entry, stored_mime_type};
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
    let mut digest = super::content_hasher();
    std::io::copy(&mut file.take(size + 1), &mut digest)
        .map_err(|_| "Could not hash captured bytes")?;
    let stored_mime = super::storage_mime(mime);
    let proof = ipc::content_proof(&digest.finalize().into(), stored_mime);
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
    use std::io::{Seek, Write};

    #[test]
    fn comparison_is_complete_bounded_and_does_not_move_the_input_cursor() {
        let mut file = tempfile::tempfile().unwrap();
        let value = vec![b'x'; 40000];
        file.write_all(&value).unwrap();
        let position = file.stream_position().unwrap();
        assert!(equal_candidate(
            &mut &value[..],
            &file,
            value.len() as u64,
            &mut 50000
        ));
        let mut changed = value.clone();
        changed[39999] = b'y';
        assert!(!equal_candidate(
            &mut &changed[..],
            &file,
            value.len() as u64,
            &mut 50000
        ));
        let mut budget = 10;
        assert!(!equal_candidate(
            &mut &value[..],
            &file,
            value.len() as u64,
            &mut budget
        ));
        assert_eq!(budget, 0);
        assert_eq!(file.stream_position().unwrap(), position);
        assert!(!equal_candidate(
            &mut &value[..100],
            &file,
            value.len() as u64,
            &mut 50000
        ));
    }
}
