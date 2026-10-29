//! Direct policy access to the embedded store; no external service control.
use async_trait::async_trait;

use crate::history_store::{HistoryStore, StorePolicy};

#[async_trait]
pub(super) trait PolicyControl: Send + Sync {
    async fn policy(&self) -> Result<StorePolicy, String>;
    async fn apply(&self, policy: StorePolicy) -> Result<(), String>;
    async fn close(&self) -> Result<(), String> {
        let mut policy = self.policy().await?;
        policy.admission = false;
        self.apply(policy).await
    }
}

#[async_trait]
impl PolicyControl for HistoryStore {
    async fn policy(&self) -> Result<StorePolicy, String> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.policy().map_err(|error| error.to_string()))
            .await
            .map_err(|_| "Store policy task failed".to_owned())?
    }

    async fn apply(&self, policy: StorePolicy) -> Result<(), String> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || {
            store
                .apply_policy(policy)
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|_| "Store policy task failed".to_owned())?
    }
}

pub(super) struct Unavailable;

#[async_trait]
impl PolicyControl for Unavailable {
    async fn policy(&self) -> Result<StorePolicy, String> {
        Err("History store is unavailable".into())
    }
    async fn apply(&self, _: StorePolicy) -> Result<(), String> {
        Err("History store is unavailable".into())
    }
    async fn close(&self) -> Result<(), String> {
        // API-only fixtures have no attached store to admit writes.
        Ok(())
    }
}
