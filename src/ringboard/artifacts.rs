use std::{
    collections::{HashMap, HashSet},
    env, fs,
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use crate::backend::{BackendError, BackendErrorKind, BackendResult, MAX_WAYLAND_SELECTION_BYTES};

use super::content::{LocalImageSource, image_identity};

#[derive(Clone)]
pub(super) struct ArtifactMatch {
    pub path: PathBuf,
    pub source_entry_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArtifactRecord {
    path: PathBuf,
    source_entry_id: String,
    image_identity: String,
    #[serde(default)]
    created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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
        let manifest_path = state_root().map(|root| root.join("clip-daemon/generated-files.json"));
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

    pub fn match_local_image(&self, source: &LocalImageSource) -> Option<ArtifactMatch> {
        let record = self.records.get(&source.path)?;
        let source_entry_id = read_registered_image(source)
            .filter(|identity| identity == &record.image_identity)
            .map(|_| record.source_entry_id.clone());
        Some(ArtifactMatch {
            path: source.path.clone(),
            source_entry_id,
        })
    }

    pub fn match_file_uris(&self, uris: impl Iterator<Item = String>) -> Option<ArtifactMatch> {
        uris.filter_map(|uri| Url::parse(&uri).ok()?.to_file_path().ok())
            .find(|path| self.records.contains_key(path))
            .map(|path| ArtifactMatch {
                path,
                source_entry_id: None,
            })
    }

    pub fn reconcile(&mut self, referenced: &HashSet<PathBuf>) -> BackendResult<usize> {
        self.prune(referenced, false)
    }

    pub fn clear_all(&mut self) -> BackendResult<usize> {
        self.active_selection = None;
        self.inline_echoes.clear();
        self.prune(&HashSet::new(), true)
    }

    fn prune(&mut self, referenced: &HashSet<PathBuf>, force: bool) -> BackendResult<usize> {
        let now = unix_time();
        let stale = self
            .records
            .iter()
            .filter(|(path, record)| {
                !referenced.contains(*path)
                    && (force
                        || (self.active_selection.as_ref() != Some(*path)
                            && now.saturating_sub(record.created_at) >= PRUNE_GRACE_SECONDS))
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        let mut removed = 0;
        for path in stale {
            let Some(file_removed) = remove_artifact(self.root.as_deref(), &path) else {
                continue;
            };
            self.records.remove(&path);
            removed += usize::from(file_removed);
        }
        self.persist()?;
        Ok(removed)
    }

    fn persist(&self) -> BackendResult<()> {
        let Some(path) = &self.manifest_path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| artifact_error("Generated-file registry path is invalid"))?;
        fs::create_dir_all(parent).map_err(artifact_error)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(artifact_error)?;
        let temp = parent.join(format!(".generated-files-{}.tmp", Uuid::new_v4()));
        let result = (|| {
            let manifest = Manifest {
                records: self.records.values().cloned().collect(),
                inline_echoes: self.inline_echoes.values().cloned().collect(),
            };
            let bytes = serde_json::to_vec_pretty(&manifest).map_err(artifact_error)?;
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
        if result.is_err() {
            let _ = fs::remove_file(temp);
        }
        result
    }
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

fn state_root() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
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

        assert_eq!(
            registry.match_inline_echo("image/png", b"edited-image", "captured-echo"),
            Some("replacement".into())
        );
        assert_eq!(
            registry.match_inline_echo("image/png", b"edited-image", "replacement"),
            None
        );

        let restored = ArtifactRegistry::load(None, Some(manifest));
        assert_eq!(
            restored.match_inline_echo("image/png", b"edited-image", "captured-echo"),
            Some("replacement".into())
        );

        let large = vec![7; super::super::INSPECTION_LIMIT + 10];
        registry
            .register_inline_echo("large-replacement", "image/png", &large)
            .unwrap();
        assert_eq!(
            registry.match_inline_echo(
                "image/png",
                &large[..super::super::INSPECTION_LIMIT],
                "large-echo"
            ),
            Some("large-replacement".into())
        );
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
        registry.clear_active_selection();
        registry.clear_all().unwrap();

        assert!(!generated.exists());
        assert!(unrelated.exists());
    }
}
