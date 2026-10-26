//! Direct in-process ingress through the same admitted service as D-Bus.
use super::{Caller, ClipDaemon, OBJECT_PATH};
use anyhow::Result;
use serde_json::Value;
use shelllist_daemon_tokio::{JsonlListener, JsonlSocketConfig, LocalJsonlService, SocketSession};
use std::sync::Arc;

#[cfg(test)]
mod tests;

struct Service(zbus::Connection);
impl LocalJsonlService for Service {
    async fn call(&self, session: &SocketSession, method: &str, params: Value) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, ClipDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        Ok(serde_json::from_str(
            &daemon
                .call_owned(method, &params.to_string(), Caller::Socket(session.clone()))
                .await,
        )?)
    }
    async fn subscribe(&self, session: &SocketSession, streams: Vec<String>) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, ClipDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        let lifetime = session.clone();
        Ok(serde_json::from_str(
            &daemon
                .subscribe_owned(
                    streams,
                    session.owner().into(),
                    session.clone().into(),
                    async move { lifetime.closed().await },
                )
                .await,
        )?)
    }
    async fn cancel(&self, session: &SocketSession, id: &str) -> Result<Value> {
        let interface = self
            .0
            .object_server()
            .interface::<_, ClipDaemon>(OBJECT_PATH)
            .await?;
        let daemon = interface.get().await;
        Ok(serde_json::from_str(
            &daemon.cancel_owned(id, Some(session.owner())).await,
        )?)
    }
    async fn disconnected(&self, session: &SocketSession) {
        if let Ok(interface) = self
            .0
            .object_server()
            .interface::<_, ClipDaemon>(OBJECT_PATH)
            .await
        {
            interface
                .get()
                .await
                .api
                .release_owner(session.owner())
                .await;
        }
    }
}
fn config() -> JsonlSocketConfig {
    JsonlSocketConfig {
        // Match the retained clipboard client's escaped-text framing allowance.
        maximum_request_bytes: shelllist_daemon_core::MAX_JSONL_REQUEST_BYTES,
        max_connections: 4,
        max_in_flight_requests: 4,
        ..Default::default()
    }
}
pub(super) async fn listen(
    connection: zbus::Connection,
    stop: impl std::future::Future<Output = ()>,
) -> Result<()> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .ok_or_else(|| anyhow::anyhow!("XDG_RUNTIME_DIR is required for the clipboard socket"))?;
    JsonlListener::bind(std::path::Path::new(&runtime), "clip-daemon")
        .await?
        .serve(
            Arc::new(Service(connection)),
            crate::client::ClipCorrelation,
            config(),
            stop,
        )
        .await
}
