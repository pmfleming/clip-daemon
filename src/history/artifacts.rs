//! SQL ownership records, not clipboard URIs, authorize generated-file deletion.
use super::content::image_identity;
use crate::{
    backend::{BackendResult, operation_error},
    history_store::{ArtifactRecord, HistoryStore},
};
use std::{
    env, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub(super) struct ArtifactRegistry {
    store: HistoryStore,
    root: Option<PathBuf>,
    active: Arc<Mutex<Option<PathBuf>>>,
}
impl ArtifactRegistry {
    pub fn new(store: HistoryStore, active: Arc<Mutex<Option<PathBuf>>>) -> Self {
        Self {
            store,
            active,
            root: env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join("Pictures/Screenshots/clipboard-history")),
        }
    }
    pub fn register(
        &self,
        path: &Path,
        source_entry_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        if !owned_path(self.root.as_deref(), path) {
            return Err(operation_error(
                "Generated file is outside daemon ownership",
            ));
        }
        let metadata = fs::symlink_metadata(path).map_err(operation_error)?;
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.nlink() != 1
        {
            return Err(operation_error("Generated file ownership is invalid"));
        }
        self.store.register_artifact(ArtifactRecord {
            path: path.to_owned(),
            source_entry_id: source_entry_id.to_owned(),
            image_identity: image_identity(mime, bytes),
            created_at: unix_time(),
            device: metadata.dev(),
            inode: metadata.ino(),
            owner: metadata.uid(),
        })?;
        // Activation happens only after successful publication; failed attempts
        // must not stop protecting the clipboard's previous generated URI.
        Ok(())
    }
    pub fn activate_if_generated(&self, path: &Path) -> BackendResult<()> {
        let generated = self
            .store
            .artifact_get(
                path.to_str()
                    .ok_or_else(|| operation_error("Generated path is not UTF-8"))?,
            )?
            .is_some();
        *self.active.lock().map_err(|_| super::lock_error())? = generated.then(|| path.to_owned());
        Ok(())
    }
    pub fn observe_selection(&self, bytes: &[u8]) -> BackendResult<()> {
        let mut active = self.active.lock().map_err(|_| super::lock_error())?;
        let Some(path) = active.as_ref() else {
            return Ok(());
        };
        let still_referenced = std::str::from_utf8(bytes).ok().is_some_and(|text| {
            text.lines().any(|line| {
                url::Url::parse(line.trim())
                    .ok()
                    .and_then(|uri| uri.to_file_path().ok())
                    .as_ref()
                    == Some(path)
            })
        });
        if !still_referenced {
            *active = None;
        }
        Ok(())
    }

    pub fn clear_active_selection(&self) -> BackendResult<()> {
        *self.active.lock().map_err(|_| super::lock_error())? = None;
        Ok(())
    }
    pub fn remove_registered(&self, path: &Path) -> BackendResult<()> {
        if let Some(record) = self.store.artifact_get(
            path.to_str()
                .ok_or_else(|| operation_error("Generated path is not UTF-8"))?,
        )? {
            self.remove(&record)?;
        }
        let mut active = self.active.lock().map_err(|_| super::lock_error())?;
        if active.as_deref() == Some(path) {
            *active = None;
        }
        Ok(())
    }
    /// Each pass is bounded; SQL excludes ALL live references, independent of UI paging.
    pub fn reconcile(&self) -> BackendResult<usize> {
        self.prune(unix_time().saturating_sub(60))
    }
    pub fn clear_all(&self) -> BackendResult<usize> {
        self.clear_active_selection()?;
        self.prune(u64::MAX)
    }
    fn prune(&self, before: u64) -> BackendResult<usize> {
        let active = self.active.lock().map_err(|_| super::lock_error())?.clone();
        let mut removed = 0;
        for record in self.store.artifact_candidates(before, 128)? {
            if active.as_deref() == Some(Path::new(&record.path)) {
                continue;
            }
            removed += usize::from(self.remove(&record)?);
        }
        Ok(removed)
    }
    fn remove(&self, record: &ArtifactRecord) -> BackendResult<bool> {
        if !owned_path(self.root.as_deref(), Path::new(&record.path)) {
            // Ownership is no longer provable. Forget metadata, never unlink;
            // otherwise stale rows could starve every bounded cleanup pass.
            self.store.forget_artifact(&record.path)?;
            return Ok(false);
        }
        let metadata = match fs::symlink_metadata(&record.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.store.forget_artifact(Path::new(&record.path))?;
                return Ok(false);
            }
            Err(error) => return Err(operation_error(error)),
        };
        // Replaced files, symlinks, directories and hard links are never unlinked.
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.dev() != record.device
            || metadata.ino() != record.inode
            || metadata.uid() != record.owner
            || record.owner != rustix::process::geteuid().as_raw()
        {
            self.store.forget_artifact(&record.path)?;
            return Ok(false);
        }
        fs::remove_file(&record.path).map_err(operation_error)?;
        self.store.forget_artifact(Path::new(&record.path))?;
        Ok(true)
    }
}
fn owned_path(root: Option<&Path>, path: &Path) -> bool {
    let Some(root) = root else { return false };
    if path.parent() != Some(root) {
        return false;
    }
    // Reject a symlinked ownership root or ancestors; never traverse into arbitrary directories.
    if fs::canonicalize(root).ok().as_deref() != Some(root) {
        return false;
    }
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return false;
    };
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return false;
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((id, extension)) = name
        .strip_prefix("clipboard-")
        .and_then(|name| name.rsplit_once('.'))
    else {
        return false;
    };
    Uuid::parse_str(id).is_ok()
        && matches!(
            extension,
            "png" | "jpg" | "webp" | "gif" | "bmp" | "tiff" | "svg"
        )
}
fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn staging_does_not_replace_active_selection_until_publication() {
        let directory = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&directory.path().join("history.db")).unwrap();
        let root = directory.path().join("generated");
        fs::create_dir(&root).unwrap();
        let registry = ArtifactRegistry {
            store,
            root: Some(root.clone()),
            active: Arc::default(),
        };
        let paths: Vec<_> = (0..2)
            .map(|_| root.join(format!("clipboard-{}.png", Uuid::new_v4())))
            .collect();
        for path in &paths {
            fs::write(path, b"image").unwrap();
        }
        registry
            .register(&paths[0], "source", "image/png", b"image")
            .unwrap();
        registry.activate_if_generated(&paths[0]).unwrap();
        registry
            .register(&paths[1], "source", "image/png", b"image")
            .unwrap();
        assert_eq!(registry.active.lock().unwrap().as_ref(), Some(&paths[0]));
        let uri = url::Url::from_file_path(&paths[0]).unwrap();
        registry.observe_selection(uri.as_str().as_bytes()).unwrap();
        assert_eq!(registry.active.lock().unwrap().as_ref(), Some(&paths[0]));
        registry
            .observe_selection(b"other clipboard content")
            .unwrap();
        assert!(registry.active.lock().unwrap().is_none());
    }

    #[test]
    fn reconciliation_never_unlinks_replacements_or_arbitrary_uris() {
        let directory = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&directory.path().join("history.db")).unwrap();
        let root = directory.path().join("generated");
        fs::create_dir(&root).unwrap();
        let registry = ArtifactRegistry {
            store,
            root: Some(root.clone()),
            active: Arc::default(),
        };
        let path = root.join(format!("clipboard-{}.png", Uuid::new_v4()));
        fs::write(&path, b"image").unwrap();
        registry
            .register(&path, "source", "image/png", b"image")
            .unwrap();
        let unrelated = root.join("unrelated.png");
        fs::write(&unrelated, b"private").unwrap();
        assert!(
            registry
                .register(&unrelated, "source", "image/png", b"private")
                .is_err()
        );
        fs::rename(&path, root.join("original")).unwrap();
        std::os::unix::fs::symlink(&unrelated, &path).unwrap();
        assert_eq!(registry.clear_all().unwrap(), 0);
        assert!(path.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&unrelated).unwrap(), b"private");
        fs::remove_file(&path).unwrap();
        fs::write(&path, b"replacement").unwrap();
        assert_eq!(registry.clear_all().unwrap(), 0);
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
        fs::remove_file(&path).unwrap();
        fs::rename(root.join("original"), &path).unwrap();
        // Replaced ownership was forgotten, never inferred from a name again.
        assert_eq!(registry.clear_all().unwrap(), 0);
        registry
            .register(&path, "source", "image/png", b"image")
            .unwrap();
        assert_eq!(registry.clear_all().unwrap(), 1);
        assert!(!path.exists());
        assert!(unrelated.exists());
    }
}
