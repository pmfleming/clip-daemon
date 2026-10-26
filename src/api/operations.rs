//! Bounded owner registry. Backend snapshots, not broadcast payloads, are truth.
use crate::{actions::ApiError, model::OperationResult};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

pub(super) const RETENTION: Duration = Duration::from_secs(300);
const MAX_ACTIVE: usize = 64;
const MAX_OWNER_ACTIVE: usize = 16;
const MAX_RECORDS: usize = 256;

pub(super) struct Record {
    pub owner: Option<String>,
    pub operation: OperationResult,
    pub revision: u64,
    pub updated: Instant,
}
impl Record {
    pub fn active(&self) -> bool {
        self.operation.status == "started"
    }
}

#[derive(Default)]
pub(super) struct Book {
    pub records: HashMap<String, Record>,
    pending: HashMap<String, Option<String>>,
}

pub(super) struct Reservation {
    book: Weak<Mutex<Book>>,
    id: String,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if let Some(book) = self.book.upgrade() {
            book.lock()
                .unwrap_or_else(|p| p.into_inner())
                .pending
                .remove(&self.id);
        }
    }
}

impl Book {
    pub fn prune(&mut self) {
        self.records
            .retain(|_, record| record.active() || record.updated.elapsed() < RETENTION);
    }

    pub fn reserve(book: &Arc<Mutex<Self>>, owner: Option<&str>) -> Result<Reservation, ApiError> {
        let mut state = book.lock().unwrap_or_else(|p| p.into_inner());
        state.prune();
        let active: Vec<_> = state
            .records
            .values()
            .filter(|record| record.active())
            .collect();
        let own = active
            .iter()
            .filter(|record| record.owner.as_deref() == owner)
            .count()
            + state
                .pending
                .values()
                .filter(|value| value.as_deref() == owner)
                .count();
        if active.len() + state.pending.len() >= MAX_ACTIVE || own >= MAX_OWNER_ACTIVE {
            return Err(ApiError::new(
                "busy",
                "Too many active clipboard operations",
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        state.pending.insert(id.clone(), owner.map(str::to_owned));
        Ok(Reservation {
            book: Arc::downgrade(book),
            id,
        })
    }

    pub fn register(&mut self, operation: OperationResult, owner: Option<String>) {
        self.prune();
        if self.records.len() >= MAX_RECORDS
            && let Some(id) = self
                .records
                .iter()
                .filter(|(_, record)| !record.active())
                .min_by_key(|(_, record)| record.updated)
                .map(|(id, _)| id.clone())
        {
            self.records.remove(&id);
        }
        self.records.insert(
            operation.id.clone(),
            Record {
                owner,
                operation,
                revision: 1,
                updated: Instant::now(),
            },
        );
    }
}

pub(super) fn may_start(method: &str, params: &serde_json::Value) -> bool {
    method == "clipboard.capture.interactive"
        || (method == "clipboard.entry.action" && params["action"] == "annotate")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn admission_retention_and_abandoned_reservations_are_bounded() {
        let book = Arc::new(Mutex::new(Book::default()));
        let reservations: Vec<_> = (0..MAX_OWNER_ACTIVE)
            .map(|_| Book::reserve(&book, Some("a")).unwrap())
            .collect();
        assert!(Book::reserve(&book, Some("a")).is_err());
        assert!(Book::reserve(&book, Some("b")).is_ok());
        drop(reservations);
        assert!(book.lock().unwrap().pending.is_empty());
        let mut state = book.lock().unwrap();
        for _ in 0..300 {
            state.register(
                OperationResult::completed("fixture", "done"),
                Some("a".into()),
            );
        }
        assert_eq!(state.records.len(), MAX_RECORDS);
        for record in state.records.values_mut() {
            record.updated = Instant::now() - RETENTION;
        }
        state.prune();
        assert!(state.records.is_empty());
    }
}
