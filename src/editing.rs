use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::model::{EntryDetails, EntryKind};

pub const MAX_EDIT_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct EditCommit {
    pub entry_id: String,
    pub revision: u64,
    pub mime: String,
    pub value: String,
}

struct EditLease {
    commit: EditCommit,
    expires: Instant,
}

#[derive(Serialize)]
pub struct EditView {
    pub id: String,
    pub entry_id: String,
    pub revision: u64,
    pub mime: String,
    pub value: String,
    pub max_bytes: usize,
    pub expires_in_ms: u64,
}

#[derive(Default)]
pub struct EditManager {
    leases: Mutex<HashMap<String, EditLease>>,
}

impl EditManager {
    pub async fn begin(&self, details: EntryDetails) -> Result<EditView, &'static str> {
        if !editable(details.entry.kind) || details.preview_truncated {
            return Err("Clipboard entry is not safely editable");
        }
        let value = details
            .text
            .ok_or("Clipboard entry is not valid UTF-8 text")?;
        if value.len() > MAX_EDIT_BYTES {
            return Err("Clipboard entry exceeds the editable text limit");
        }
        let id = format!("edit-{}", Uuid::new_v4());
        let view = EditView {
            id: id.clone(),
            entry_id: details.entry.id.clone(),
            revision: details.entry.revision,
            mime: details.entry.mime.clone(),
            value: value.clone(),
            max_bytes: MAX_EDIT_BYTES,
            expires_in_ms: 60_000,
        };
        self.leases.lock().await.insert(
            id,
            EditLease {
                commit: EditCommit {
                    entry_id: details.entry.id,
                    revision: details.entry.revision,
                    mime: details.entry.mime,
                    value,
                },
                expires: Instant::now() + Duration::from_secs(60),
            },
        );
        Ok(view)
    }

    pub async fn commit(&self, id: &str, value: String) -> Result<EditCommit, &'static str> {
        let mut lease = self
            .leases
            .lock()
            .await
            .remove(id)
            .ok_or("Edit session is unknown or already used")?;
        if lease.expires <= Instant::now() {
            return Err("Edit session expired");
        }
        if value.len() > MAX_EDIT_BYTES {
            return Err("Edited text exceeds the configured limit");
        }
        lease.commit.value = value;
        Ok(lease.commit)
    }

    pub async fn cancel(&self, id: &str) -> bool {
        self.leases.lock().await.remove(id).is_some()
    }

    pub async fn clear(&self) {
        self.leases.lock().await.clear();
    }
}

fn editable(kind: EntryKind) -> bool {
    matches!(
        kind,
        EntryKind::Text | EntryKind::Link | EntryKind::Html | EntryKind::Json | EntryKind::Color
    )
}

#[cfg(test)]
mod tests {
    use crate::model::{EntryDetails, EntryKind, EntrySummary};

    use super::EditManager;

    fn details(kind: EntryKind, text: Option<&str>) -> EntryDetails {
        EntryDetails {
            entry: EntrySummary {
                id: "entry".into(),
                revision: 1,
                kind,
                mime: "text/plain".into(),
                byte_size: 4,
                favorite: false,
                current: true,
                preview: "test".into(),
            },
            text: text.map(str::to_owned),
            files: vec![],
            image: None,
            preview_truncated: false,
        }
    }

    #[tokio::test]
    async fn text_edits_are_one_use_and_binary_is_rejected() {
        let manager = EditManager::default();
        let edit = manager
            .begin(details(EntryKind::Text, Some("old")))
            .await
            .unwrap();
        let commit = manager.commit(&edit.id, "new".into()).await.unwrap();
        assert_eq!(commit.value, "new");
        assert!(manager.commit(&edit.id, "again".into()).await.is_err());
        assert!(
            manager
                .begin(details(EntryKind::Binary, None))
                .await
                .is_err()
        );
    }
}
