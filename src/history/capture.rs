//! Capture submissions enter the same bounded SQL worker as actions and policy changes.
use super::HistoryBackend;
use crate::{capture::CaptureSink, selection::read_selection};
use std::{
    fs::File,
    io::{Read, Seek},
};

impl CaptureSink for HistoryBackend {
    fn ready(&self, max_bytes: u64) -> Result<(), String> {
        self.ensure_running().map_err(|error| error.to_string())?;
        let policy = self
            .store()
            .and_then(|store| store.policy())
            .map_err(|error| error.to_string())?;
        // Settings opens admission only after the capture barrier is ready.
        if policy.max_entry_bytes != max_bytes {
            return Err("Capture limit does not match the history policy".into());
        }
        Ok(())
    }
    fn set_admission(&self, enabled: bool) -> Result<(), String> {
        if enabled {
            self.ensure_running().map_err(|error| error.to_string())?;
        }
        let store = self.store().map_err(|error| error.to_string())?;
        let mut policy = store.policy().map_err(|error| error.to_string())?;
        policy.admission = enabled;
        store
            .apply_policy(policy)
            .map_err(|error| error.to_string())
    }
    fn ingest(&self, mime: &str, mut file: &File) -> Result<(), String> {
        self.ensure_running().map_err(|error| error.to_string())?;
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| "History transaction unavailable")?;
        let store = self.store().map_err(|error| error.to_string())?;
        let policy = store.policy().map_err(|error| error.to_string())?;
        if !policy.admission {
            return Ok(());
        }
        let size = file
            .metadata()
            .map_err(|_| "Could not inspect captured bytes")?
            .len();
        if size == 0 || size > policy.max_entry_bytes {
            return Ok(());
        }
        file.rewind()
            .map_err(|_| "Could not rewind captured bytes")?;
        let bytes = read_selection(&mut file, size, policy.max_entry_bytes)
            .map_err(|error| error.to_string())?;
        // Detect growth rather than persisting a prefix. Capture files are already bounded.
        let mut extra = [0];
        if bytes.len() as u64 != size
            || file
                .read(&mut extra)
                .map_err(|_| "Could not inspect captured bytes")?
                != 0
        {
            return Err("Captured bytes changed during admission".into());
        }
        let mime = match mime {
            "UTF8_STRING" | "STRING" | "TEXT" => "text/plain",
            other => other,
        };
        match store.insert(mime, &bytes) {
            Ok(_) => self
                .artifact_registry()
                .and_then(|registry| registry.observe_selection(&bytes))
                .map_err(|error| error.to_string()),
            Err(error) => {
                // Closing admission concurrently is normal during pause/wipe/shutdown.
                if !store.policy().map_err(|error| error.to_string())?.admission {
                    Ok(())
                } else {
                    Err(error.to_string())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn capture_requires_admission_and_deduplicates() {
        let directory = tempfile::tempdir().unwrap();
        let store =
            crate::history_store::HistoryStore::open(&directory.path().join("history.db")).unwrap();
        let backend = HistoryBackend::from_store(store.clone());
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"capture").unwrap();
        backend.ingest("text/plain", &file).unwrap();
        let query = crate::backend::HistoryQuery {
            query: String::new(),
            generation: 1,
            offset: 0,
            limit: 100,
            collapse_self_echoes: true,
        };
        assert!(store.query(query.clone()).unwrap().entries.is_empty());
        let mut policy = store.policy().unwrap();
        policy.admission = true;
        store.apply_policy(policy).unwrap();
        backend.ingest("UTF8_STRING", &file).unwrap();
        backend.ingest("text/plain", &file).unwrap();
        assert_eq!(store.query(query).unwrap().entries.len(), 1);
    }
}
