use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::OpenOptions,
    io::{BufRead, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use shelllist_daemon_core::{XdgRoot, resolve_xdg_root};
use url::Url;
use uuid::Uuid;

use crate::backend::{BackendError, BackendErrorKind, BackendResult, MAX_WAYLAND_SELECTION_BYTES};

use super::content::{LocalImageSource, image_identity};

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactRecord {
    path: PathBuf,
    source_entry_id: String,
    image_identity: String,
    #[serde(default)]
    created_at: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct InlineEchoRecord {
    source_entry_id: String,
    image_identity: String,
    #[serde(default)]
    created_at: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    records: Vec<ArtifactRecord>,
    #[serde(default)]
    inline_echoes: Vec<InlineEchoRecord>,
}

#[derive(Serialize)]
struct ManifestView<'a> {
    records: Vec<&'a ArtifactRecord>,
    inline_echoes: Vec<&'a InlineEchoRecord>,
}

/// Tracks only files created by clip-daemon. Cleanup never infers ownership
/// from a directory listing or removes an unregistered path.
pub(super) struct ArtifactRegistry {
    root: Option<PathBuf>,
    manifest_path: Option<PathBuf>,
    records: HashMap<PathBuf, ArtifactRecord>,
    inline_echoes: HashMap<String, InlineEchoRecord>,
    active_selection: Option<PathBuf>,
}

impl Default for ArtifactRegistry {
    fn default() -> Self {
        let root = generated_root();
        let manifest_path = resolve_xdg_root(XdgRoot::State)
            .map(|root| root.join("clip-daemon/generated-files.json"));
        Self::load(root, manifest_path)
    }
}

impl ArtifactRegistry {
    fn load(root: Option<PathBuf>, manifest_path: Option<PathBuf>) -> Self {
        let manifest = manifest_path
            .as_deref()
            .and_then(|path| fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
            .unwrap_or_default();
        let records = manifest
            .records
            .into_iter()
            .filter(|record| owned_path(root.as_deref(), &record.path))
            .map(|record| (record.path.clone(), record))
            .collect();
        let inline_echoes = manifest
            .inline_echoes
            .into_iter()
            .map(|record| (record.image_identity.clone(), record))
            .collect();
        Self {
            root,
            manifest_path,
            records,
            inline_echoes,
            active_selection: None,
        }
    }

    pub fn register(
        &mut self,
        path: &Path,
        source_entry_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        if !owned_path(self.root.as_deref(), path) {
            return Err(artifact_error(
                "Generated clipboard file path is outside daemon ownership",
            ));
        }
        let record = ArtifactRecord {
            path: path.to_owned(),
            source_entry_id: source_entry_id.to_owned(),
            image_identity: image_identity(mime, bytes),
            created_at: unix_time(),
        };
        self.records.insert(path.to_owned(), record);
        self.active_selection = Some(path.to_owned());
        if let Err(error) = self.persist() {
            self.records.remove(path);
            self.active_selection = None;
            return Err(error);
        }
        Ok(())
    }

    pub fn forget(&mut self, path: &Path) {
        self.records.remove(path);
        if self.active_selection.as_deref() == Some(path) {
            self.active_selection = None;
        }
        let _ = self.persist();
    }

    pub fn activate_if_generated(&mut self, path: &Path) {
        self.active_selection = self.records.contains_key(path).then(|| path.to_owned());
    }

    pub fn clear_active_selection(&mut self) {
        self.active_selection = None;
    }

    pub fn register_inline_echo(
        &mut self,
        source_entry_id: &str,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<()> {
        let identity = inline_image_identity(mime, bytes);
        self.inline_echoes.insert(
            identity.clone(),
            InlineEchoRecord {
                source_entry_id: source_entry_id.to_owned(),
                image_identity: identity,
                created_at: unix_time(),
            },
        );
        if self.inline_echoes.len() > MAX_INLINE_ECHOES
            && let Some(oldest) = self
                .inline_echoes
                .values()
                .min_by_key(|record| record.created_at)
                .map(|record| record.image_identity.clone())
        {
            self.inline_echoes.remove(&oldest);
        }
        self.persist()
    }

    pub fn match_inline_echo(&self, mime: &str, bytes: &[u8], entry_id: &str) -> Option<String> {
        let record = self
            .inline_echoes
            .get(&inline_image_identity(mime, bytes))?;
        (record.source_entry_id != entry_id).then(|| record.source_entry_id.clone())
    }

    pub fn match_local_image(&self, source: &LocalImageSource) -> Option<String> {
        let record = self.records.get(&source.path)?;
        read_registered_image(source)
            .filter(|identity| identity == &record.image_identity)
            .map(|_| record.source_entry_id.clone())
    }

    /// Ownership references are not UI previews: inspect every URI, not only
    /// the first match, first 100 files, or first preview-sized prefix.
    pub fn references_in(&self, mut source: impl BufRead) -> BackendResult<HashSet<PathBuf>> {
        if self.records.is_empty() {
            return Ok(HashSet::new());
        }
        const MAX_LINE_BYTES: u64 = 32 * 1024;
        let mut references = HashSet::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let length = source
                .by_ref()
                .take(MAX_LINE_BYTES)
                .read_until(b'\n', &mut line)
                .map_err(artifact_error)?;
            if length == 0 {
                break;
            }
            if length as u64 == MAX_LINE_BYTES && !line.ends_with(b"\n") {
                // An uninspectable line must never authorize deletion. Keep all
                // registered files until the ambiguous history entry is gone.
                return Ok(self.records.keys().cloned().collect());
            }
            let path = std::str::from_utf8(&line)
                .ok()
                .and_then(|line| Url::parse(line.trim()).ok())
                .and_then(|uri| uri.to_file_path().ok());
            if let Some(path) = path.filter(|path| self.records.contains_key(path)) {
                references.insert(path);
            }
        }
        Ok(references)
    }

    pub fn reconcile(&mut self, referenced: &HashSet<PathBuf>) -> BackendResult<usize> {
        self.prune(referenced, PRUNE_GRACE_SECONDS)
    }

    pub fn clear_all(&mut self) -> BackendResult<usize> {
        self.active_selection = None;
        self.inline_echoes.clear();
        self.prune(&HashSet::new(), 0)
    }

    fn prune(&mut self, referenced: &HashSet<PathBuf>, minimum_age: u64) -> BackendResult<usize> {
        let now = unix_time();
        let mut removed = 0;
        self.records.retain(|path, record| {
            let protected = referenced.contains(path)
                || self.active_selection.as_ref() == Some(path)
                || now.saturating_sub(record.created_at) < minimum_age;
            if protected {
                return true;
            }
            let Some(file_removed) = remove_artifact(self.root.as_deref(), path) else {
                return true;
            };
            removed += usize::from(file_removed);
            false
        });
        self.persist()?;
        Ok(removed)
    }

    fn persist(&self) -> BackendResult<()> {
        let Some(path) = &self.manifest_path else {
            return Ok(());
        };
        persist_manifest(
            path,
            &ManifestView {
                records: self.records.values().collect(),
                inline_echoes: self.inline_echoes.values().collect(),
            },
        )
    }
}

fn persist_manifest(path: &Path, manifest: &impl Serialize) -> BackendResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| artifact_error("Generated-file registry path is invalid"))?;
    fs::create_dir_all(parent).map_err(artifact_error)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(artifact_error)?;
    let temp = parent.join(format!(".generated-files-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let bytes = serde_json::to_vec_pretty(manifest).map_err(artifact_error)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temp)
            .map_err(artifact_error)?;
        file.write_all(&bytes).map_err(artifact_error)?;
        file.sync_all().map_err(artifact_error)?;
        fs::rename(&temp, path).map_err(artifact_error)
    })();
    result.inspect_err(|_| {
        let _ = fs::remove_file(temp);
    })
}

fn remove_artifact(root: Option<&Path>, path: &Path) -> Option<bool> {
    if !owned_path(root, path) {
        return Some(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

fn inline_image_identity(mime: &str, bytes: &[u8]) -> String {
    image_identity(mime, &bytes[..bytes.len().min(super::INSPECTION_LIMIT)])
}

fn read_registered_image(source: &LocalImageSource) -> Option<String> {
    let metadata = source.path.symlink_metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_WAYLAND_SELECTION_BYTES {
        return None;
    }
    let bytes = fs::read(&source.path).ok()?;
    Some(image_identity(source.mime, &bytes))
}

fn owned_path(root: Option<&Path>, path: &Path) -> bool {
    let Some(root) = root else { return false };
    if path.parent() != Some(root) {
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

fn generated_root() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Pictures/Screenshots/clipboard-history"))
}

const PRUNE_GRACE_SECONDS: u64 = 60;
const MAX_INLINE_ECHOES: usize = 128;

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn artifact_error(error: impl std::fmt::Display) -> BackendError {
    BackendError::new(BackendErrorKind::OperationFailed, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::ArtifactRegistry;

    #[test]
    fn annotation_echoes_match_duplicates_but_not_the_replacement_source() {
        let state = tempfile::tempdir().unwrap();
        let manifest = state.path().join("manifest.json");
        let mut registry = ArtifactRegistry::load(None, Some(manifest.clone()));
        registry
            .register_inline_echo("replacement", "image/png", b"edited-image")
            .unwrap();

        let echo = |registry: &ArtifactRegistry, entry| {
            registry.match_inline_echo("image/png", b"edited-image", entry)
        };
        assert_eq!(echo(&registry, "captured-echo"), Some("replacement".into()));
        assert_eq!(echo(&registry, "replacement"), None);

        let restored = ArtifactRegistry::load(None, Some(manifest));
        assert_eq!(echo(&restored, "captured-echo"), Some("replacement".into()));

        let large = vec![7; super::super::INSPECTION_LIMIT + 10];
        registry
            .register_inline_echo("large-replacement", "image/png", &large)
            .unwrap();
        let matched = registry.match_inline_echo(
            "image/png",
            &large[..super::super::INSPECTION_LIMIT],
            "large-echo",
        );
        assert_eq!(matched, Some("large-replacement".into()));
    }

    #[test]
    fn all_uri_references_are_retained_beyond_preview_and_file_limits() {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = ArtifactRegistry::load(Some(directory.path().into()), None);
        let paths: Vec<_> = (0..3)
            .map(|_| {
                directory
                    .path()
                    .join(format!("clipboard-{}.png", uuid::Uuid::new_v4()))
            })
            .collect();
        for path in &paths {
            std::fs::write(path, b"image").unwrap();
            registry
                .register(path, "source", "image/png", b"image")
                .unwrap();
            registry.records.get_mut(path).unwrap().created_at = 0;
        }
        registry.clear_active_selection();
        let mut payload = "# ignored comment\n".repeat(5000);
        for path in &paths[..2] {
            payload.push_str(url::Url::from_file_path(path).unwrap().as_str());
            payload.push_str("\r\n");
        }
        let referenced = registry.references_in(payload.as_bytes()).unwrap();
        assert_eq!(referenced.len(), 2);
        assert_eq!(registry.reconcile(&referenced).unwrap(), 1);
        assert!(paths[0].exists() && paths[1].exists());
        assert!(!paths[2].exists());
        let uninspectable = vec![b'x'; 40_000];
        assert_eq!(registry.references_in(&uninspectable[..]).unwrap().len(), 2);
    }

    #[test]
    fn cleanup_removes_only_registered_unreferenced_files() {
        let directory = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let generated = directory
            .path()
            .join(format!("clipboard-{}.png", uuid::Uuid::new_v4()));
        let unrelated = directory.path().join("unrelated.png");
        std::fs::write(&generated, b"image").unwrap();
        std::fs::write(&unrelated, b"image").unwrap();
        let mut registry = ArtifactRegistry::load(
            Some(directory.path().to_owned()),
            Some(state.path().join("manifest.json")),
        );
        registry
            .register(&generated, "entry-source", "image/png", b"image")
            .unwrap();
        let empty = std::collections::HashSet::new();
        registry.clear_active_selection();
        assert_eq!(registry.reconcile(&empty).unwrap(), 0); // grace period
        registry.records.get_mut(&generated).unwrap().created_at = 0;
        registry.activate_if_generated(&generated);
        assert_eq!(registry.reconcile(&empty).unwrap(), 0); // active selection
        registry.clear_active_selection();
        let referenced = std::collections::HashSet::from([generated.clone()]);
        assert_eq!(registry.reconcile(&referenced).unwrap(), 0);
        assert_eq!(registry.reconcile(&empty).unwrap(), 1);
        assert!(registry.records.is_empty());

        // Failed deletions stay registered for retry; missing files are forgotten.
        registry
            .register(&generated, "source", "image/png", b"image")
            .unwrap();
        std::fs::create_dir(&generated).unwrap();
        assert_eq!(registry.clear_all().unwrap(), 0);
        assert!(registry.records.contains_key(&generated));
        std::fs::remove_dir(&generated).unwrap();
        assert_eq!(registry.clear_all().unwrap(), 0);
        assert!(registry.records.is_empty());
        std::fs::write(&generated, b"image").unwrap();
        registry
            .register(&generated, "source", "image/png", b"image")
            .unwrap();
        assert_eq!(registry.clear_all().unwrap(), 1); // bypass active selection and grace
        assert!(!generated.exists());
        assert!(unrelated.exists());
    }
}
