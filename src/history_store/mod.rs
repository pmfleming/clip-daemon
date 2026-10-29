//! Serialized, bounded relational history storage. No history projection is cached.
mod database;
mod schema;
#[cfg(test)]
mod tests;

use crate::{
    backend::{BackendError, BackendResult, EntryTarget},
    model::{EntrySummary, HistoryPage, HistoryQuery},
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
};

pub const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
pub const QUEUE_CAPACITY: usize = 32;
pub const QUEUE_BYTE_BUDGET: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePolicy {
    pub admission: bool,
    pub max_entries: u32,
    pub max_favorites: u32,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
}
impl Default for StorePolicy {
    fn default() -> Self {
        Self {
            admission: false,
            max_entries: 1000,
            max_favorites: 100,
            max_entry_bytes: 16 * 1024 * 1024,
            max_total_bytes: 256 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone)]
pub struct StoredEntry {
    pub summary: EntrySummary,
    /// Captured MIME, distinct from the UI's resolved semantic image/file MIME.
    pub mime: String,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactRecord {
    pub path: PathBuf,
    pub source_entry_id: String,
    pub image_identity: String,
    pub created_at: u64,
    pub device: u64,
    pub inode: u64,
    pub owner: u32,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineEchoRecord {
    pub source_entry_id: String,
    pub image_identity: String,
    pub created_at: u64,
}

type Job = Box<dyn FnOnce(&mut database::Database) + Send>;
struct Submission {
    sender: Option<mpsc::SyncSender<Job>>,
    bytes: usize,
}
struct Inner {
    submission: Arc<Mutex<Submission>>,
    worker: Mutex<Option<thread::JoinHandle<BackendResult<()>>>>,
    shutdown_error: Mutex<Option<(crate::backend::BackendErrorKind, String)>>,
}
#[derive(Clone)]
pub struct HistoryStore {
    inner: Arc<Inner>,
}
struct Reservation {
    submission: Arc<Mutex<Submission>>,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut state) = self.submission.lock() {
            state.bytes -= self.bytes;
        }
    }
}
impl Inner {
    fn shutdown(&self) -> BackendResult<()> {
        // Serialize joins as well as closure so every shutdown caller waits for checkpoint.
        let mut worker = self
            .worker
            .lock()
            .map_err(|_| BackendError::unavailable("History worker lock failed"))?;
        self.submission
            .lock()
            .map_err(|_| BackendError::unavailable("History submission lock failed"))?
            .sender
            .take();
        if let Some((kind, message)) = self
            .shutdown_error
            .lock()
            .map_err(|_| BackendError::unavailable("History shutdown status lock failed"))?
            .as_ref()
        {
            return Err(BackendError::new(*kind, message.clone()));
        }
        let result = match worker.take() {
            Some(worker) => worker
                .join()
                .unwrap_or_else(|_| Err(BackendError::unavailable("History worker failed"))),
            None => Ok(()),
        };
        if let Err(error) = &result {
            *self
                .shutdown_error
                .lock()
                .map_err(|_| BackendError::unavailable("History shutdown status lock failed"))? =
                Some((error.kind, error.to_string()));
        }
        result
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
impl HistoryStore {
    pub fn open(path: &Path) -> BackendResult<Self> {
        let path = path.to_owned();
        let (sender, receiver) = mpsc::sync_channel::<Job>(QUEUE_CAPACITY);
        let (ready, initialized) = mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("clipboard-storage".into())
            .spawn(move || {
                let mut db = match database::Database::open(&path) {
                    Ok(db) => {
                        let _ = ready.send(Ok(()));
                        db
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return Ok(());
                    }
                };
                for job in receiver {
                    job(&mut db);
                }
                db.shutdown()
            })
            .map_err(crate::backend::operation_error)?;
        match initialized
            .recv()
            .map_err(|_| BackendError::unavailable("History worker failed during startup"))?
        {
            Ok(()) => Ok(Self {
                inner: Arc::new(Inner {
                    submission: Arc::new(Mutex::new(Submission {
                        sender: Some(sender),
                        bytes: 0,
                    })),
                    worker: Mutex::new(Some(worker)),
                    shutdown_error: Mutex::new(None),
                }),
            }),
            Err(error) => {
                let _ = worker.join();
                Err(error)
            }
        }
    }
    pub fn open_default() -> BackendResult<Self> {
        let root = std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute())
                    .map(|p| p.join(".local/share"))
            })
            .ok_or_else(|| BackendError::unavailable("Clipboard data directory is unavailable"))?;
        Self::open(&root.join("clip-daemon/history/history.db"))
    }
    // Reserve before copying caller payloads. Both queued and executing input bytes count.
    fn reserve(&self, bytes: usize) -> BackendResult<Reservation> {
        let mut state = self
            .inner
            .submission
            .lock()
            .map_err(|_| BackendError::unavailable("History submission lock failed"))?;
        if state.sender.is_none() {
            return Err(BackendError::unavailable("History storage is closed"));
        }
        if bytes > QUEUE_BYTE_BUDGET.saturating_sub(state.bytes) {
            return Err(BackendError::unavailable(
                "History input byte budget exhausted",
            ));
        }
        state.bytes += bytes;
        Ok(Reservation {
            submission: self.inner.submission.clone(),
            bytes,
        })
    }
    fn submit<T: Send + 'static>(
        &self,
        reservation: Reservation,
        f: impl FnOnce(&mut database::Database) -> BackendResult<T> + Send + 'static,
    ) -> BackendResult<T> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let job: Job = Box::new(move |db| {
            let result = if db.blocked {
                Err(BackendError::unavailable(
                    "History storage is fenced; restart required",
                ))
            } else {
                f(db)
            };
            drop(reservation);
            let _ = sender.send(result);
        });
        // Drop rejected jobs outside the mutex: their reservations acquire it.
        let send = {
            let state = self
                .inner
                .submission
                .lock()
                .map_err(|_| BackendError::unavailable("History submission lock failed"))?;
            match &state.sender {
                Some(sender) => sender.try_send(job),
                None => Err(mpsc::TrySendError::Disconnected(job)),
            }
        };
        send.map_err(|_| BackendError::unavailable("History queue is full or closed"))?;
        receiver
            .recv()
            .map_err(|_| BackendError::unavailable("History worker stopped"))?
    }
    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut database::Database) -> BackendResult<T> + Send + 'static,
    ) -> BackendResult<T> {
        self.submit(self.reserve(0)?, f)
    }
    pub fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        if query.query.len() > crate::backend::MAX_QUERY_BYTES {
            return Err(crate::backend::invalid_data("Search query is too long"));
        }
        self.call(move |db| db.query(query))
    }
    pub fn get(&self, id: &str, expected: Option<u64>) -> BackendResult<StoredEntry> {
        validate_id(id)?;
        let id = id.to_owned();
        self.call(move |db| db.get(&id, expected))
    }
    pub fn revision(&self) -> BackendResult<u64> {
        self.call(|db| db.revision())
    }
    pub fn insert(&self, mime: &str, bytes: &[u8]) -> BackendResult<EntrySummary> {
        validate_input(mime, bytes)?;
        let reservation = self.reserve(bytes.len())?;
        let (mime, bytes) = (mime.to_owned(), bytes.to_vec());
        self.submit(reservation, move |db| db.insert(&mime, &bytes))
    }
    pub fn replace(
        &self,
        id: &str,
        expected: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<EntrySummary> {
        validate_id(id)?;
        validate_input(mime, bytes)?;
        let reservation = self.reserve(bytes.len())?;
        let (id, mime, bytes) = (id.to_owned(), mime.to_owned(), bytes.to_vec());
        self.submit(reservation, move |db| {
            db.replace(&id, expected, &mime, &bytes)
        })
    }
    pub fn favorite(&self, id: &str, expected: u64, value: bool) -> BackendResult<()> {
        validate_id(id)?;
        let id = id.to_owned();
        self.call(move |db| db.favorite(&id, expected, value))
    }
    pub fn remove_many(&self, targets: &[EntryTarget]) -> BackendResult<()> {
        if targets.len() > 10_000 {
            return Err(crate::backend::invalid_data("Too many delete targets"));
        }
        for target in targets {
            validate_id(&target.opaque_id)?;
        }
        let reservation = self.reserve(
            targets
                .iter()
                .map(|target| target.opaque_id.len() + std::mem::size_of::<EntryTarget>())
                .sum(),
        )?;
        let targets = targets.to_vec();
        self.submit(reservation, move |db| db.remove_many(&targets))
    }
    pub fn wipe(&self) -> BackendResult<()> {
        self.call(|db| db.wipe())
    }
    pub fn shutdown(&self) -> BackendResult<()> {
        self.inner.shutdown()
    }
    pub fn apply_policy(&self, policy: StorePolicy) -> BackendResult<()> {
        self.call(move |db| db.apply_policy(policy))
    }
    pub fn policy(&self) -> BackendResult<StorePolicy> {
        self.call(|db| db.policy())
    }
    pub fn artifact_register(&self, record: ArtifactRecord) -> BackendResult<()> {
        self.register_artifact(record)
    }
    pub fn artifact_forget(&self, path: impl AsRef<Path>) -> BackendResult<()> {
        self.forget_artifact(path.as_ref())
    }
    pub fn artifact_get(&self, path: impl AsRef<Path>) -> BackendResult<Option<ArtifactRecord>> {
        validate_path(path.as_ref())?;
        let path = path
            .as_ref()
            .to_str()
            .ok_or_else(|| crate::backend::invalid_data("Artifact path must be UTF-8"))?
            .to_owned();
        self.call(move |db| db.artifact_get(&path))
    }
    pub fn artifact_candidates(
        &self,
        before: u64,
        limit: usize,
    ) -> BackendResult<Vec<ArtifactRecord>> {
        self.call(move |db| db.artifact_candidates(before, limit))
    }
    pub fn artifacts(&self) -> BackendResult<Vec<ArtifactRecord>> {
        self.call(|db| db.artifacts())
    }
    pub fn register_artifact(&self, record: ArtifactRecord) -> BackendResult<()> {
        validate_path(&record.path)?;
        validate_id(&record.source_entry_id)?;
        if record.image_identity.len() > 256 {
            return Err(crate::backend::invalid_data(
                "Artifact identity is too long",
            ));
        }
        let reservation = self.reserve(
            record.path.as_os_str().len()
                + record.source_entry_id.len()
                + record.image_identity.len(),
        )?;
        self.submit(reservation, move |db| db.register_artifact(record))
    }
    pub fn forget_artifact(&self, path: &Path) -> BackendResult<()> {
        validate_path(path)?;
        let path = path.to_owned();
        self.call(move |db| db.forget_artifact(&path))
    }
    pub fn artifact_references(&self) -> BackendResult<HashSet<PathBuf>> {
        self.call(|db| db.artifact_references())
    }
    pub fn inline_echoes(&self) -> BackendResult<Vec<InlineEchoRecord>> {
        self.call(|db| db.inline_echoes())
    }
    pub fn register_inline_echo(&self, record: InlineEchoRecord) -> BackendResult<()> {
        validate_id(&record.source_entry_id)?;
        if record.image_identity.len() > 256 {
            return Err(crate::backend::invalid_data(
                "Artifact identity is too long",
            ));
        }
        self.call(move |db| db.register_inline_echo(record))
    }
    pub fn clear_inline_echoes(&self) -> BackendResult<()> {
        self.call(|db| db.clear_inline_echoes())
    }
}
fn validate_id(id: &str) -> BackendResult<()> {
    if id.len() > 128 {
        return Err(crate::backend::invalid_data(
            "Clipboard entry ID is too long",
        ));
    }
    Ok(())
}
fn validate_path(path: &Path) -> BackendResult<()> {
    if path.as_os_str().len() > 32768 {
        return Err(crate::backend::invalid_data("Artifact path is too long"));
    }
    Ok(())
}
fn validate_input(mime: &str, bytes: &[u8]) -> BackendResult<()> {
    if mime.len() > 255 || mime.contains('\0') || bytes.len() as u64 > MAX_ENTRY_BYTES {
        return Err(crate::backend::invalid_data(
            "Clipboard input exceeds storage limits",
        ));
    }
    Ok(())
}
