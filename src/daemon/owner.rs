//! D-Bus resource lifetime, including work that finishes after its caller leaves.
//! Background image jobs deliberately keep their existing lifecycle in this stage.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_tokio::{OwnerLossError, OwnerLossMonitor, TaskGroup};

use crate::api::ApiService;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub(super) struct Owners {
    monitor: OwnerLossMonitor,
    stopped: Arc<AtomicBool>,
}

impl Owners {
    pub async fn start(
        connection: zbus::Connection,
        api: Arc<ApiService>,
        tasks: &TaskGroup,
    ) -> Result<Self> {
        let monitor = OwnerLossMonitor::new(connection);
        // Establish observation before exporting the service, not after the
        // first resource has already been allocated.
        let mut losses = monitor.subscribe().await?;
        let stopped = Arc::new(AtomicBool::new(false));
        let owners = Self { monitor, stopped };
        let watcher = owners.clone();
        tasks.spawn("clipboard-resource-owners", async move {
            loop {
                match losses.recv().await {
                    Ok(owner) => api.release_owner(&owner).await,
                    Err(OwnerLossError::Lagged(_)) => {
                        for owner in api.resource_owners().await {
                            if !watcher.alive(&owner).await {
                                api.release_owner(&owner).await;
                            }
                        }
                    }
                    Err(OwnerLossError::Stopped) => {
                        watcher.shutdown(&api).await;
                        return;
                    }
                }
            }
        });
        Ok(owners)
    }

    pub async fn shutdown(&self, api: &ApiService) {
        self.stopped.store(true, Ordering::Release);
        for owner in api.resource_owners().await {
            api.release_owner(&owner).await;
        }
    }

    async fn alive(&self, owner: &str) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && self.monitor.has_owner(owner).await.unwrap_or(false)
            && !self.stopped.load(Ordering::Acquire)
    }

    pub async fn dispatch(
        &self,
        api: &ApiService,
        method: &str,
        params: Value,
        owner: String,
    ) -> Value {
        if !self.alive(&owner).await {
            api.release_owner(&owner).await;
            return crate::api::error(
                "owner-unavailable",
                "Clipboard caller is unavailable".into(),
            );
        }
        let response = api
            .dispatch_owned(method, params, Some(owner.clone()))
            .await;
        // A loss notification may have run before an admitted call registered
        // its session/edit/challenge. Checking after completion closes that race.
        // Do not abort or replay a mutation whose effects may have committed.
        if !self.alive(&owner).await {
            api.release_owner(&owner).await;
        }
        response
    }
}
