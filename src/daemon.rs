use std::sync::{Arc, atomic::AtomicU64};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use shelllist_daemon_tokio::OwnedTaskRegistry;
use zbus::{connection, message::Header, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService},
    backend::ClipboardBackend,
};

mod owner;
mod subscription;

pub const BUS_NAME: &str = "org.laufan.ClipDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/ClipDaemon";
pub const INTERFACE: &str = "org.laufan.ClipDaemon2";

pub struct ClipDaemon {
    boundary: shelllist_daemon_tokio::ServiceBoundary,
    api: Arc<ApiService>,
    owners: owner::Owners,
    history_events: tokio::sync::broadcast::Sender<subscription::HistoryUpdate>,
    subscriptions: Arc<OwnedTaskRegistry>,
}

impl ClipDaemon {
    fn new(
        api: Arc<ApiService>,
        history_events: tokio::sync::broadcast::Sender<subscription::HistoryUpdate>,
        owners: owner::Owners,
    ) -> Self {
        Self {
            // Large clipboard bodies are intentional; bound their concurrency as well.
            boundary: shelllist_daemon_tokio::ServiceBoundary::with_limits(
                shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
                4,
                16,
                shelllist_daemon_core::MAX_JSONL_REQUEST_BYTES,
                shelllist_daemon_core::MAX_JSONL_REQUEST_BYTES,
            ),
            api,
            owners,
            history_events,
            subscriptions: Arc::new(OwnedTaskRegistry::default()),
        }
    }
}

#[zbus::interface(name = "org.laufan.ClipDaemon2")]
impl ClipDaemon {
    async fn call(
        &self,
        method: &str,
        params_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> String {
        let admitted =
            match self
                .boundary
                .call(header.sender().map(|s| s.as_str()), method, params_json)
            {
                Ok(admitted) => admitted,
                Err(response) => return response,
            };
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(error) => {
                return api::error("validation-error", format!("invalid params JSON: {error}"))
                    .to_string();
            }
        };
        let Some(owner) = header.sender().map(ToString::to_string) else {
            return api::error(
                "owner-unavailable",
                "Clipboard caller is unavailable".into(),
            )
            .to_string();
        };
        let owners = self.owners.clone();
        let api = self.api.clone();
        let method = method.to_owned();
        // The existing unclassified boundary has no execution policy for run().
        // Keep its admission and cleanup alive even if the D-Bus waiter leaves.
        tokio::spawn(async move {
            admitted.encode(&owners.dispatch(&api, &method, params, owner).await)
        })
        .await
        .unwrap_or_else(|_| {
            api::error(
                "outcome-unknown",
                "Clipboard execution outcome is unknown; do not replay".into(),
            )
            .to_string()
        })
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        let admitted = match self
            .boundary
            .subscribe(header.sender().map(|s| s.as_str()), &streams)
        {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let Some(owner) = header.sender().map(|sender| sender.to_owned()) else {
            return api::error(
                "subscription-unavailable",
                "D-Bus caller identity is unavailable".into(),
            )
            .to_string();
        };
        admitted.encoded(subscription::start(self, streams, owner, emitter).await)
    }

    /// Publish raw bytes while keeping Wayland selection ownership in the daemon.
    async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        #[zbus(header)] header: Header<'_>,
    ) -> String {
        let admitted =
            match self
                .boundary
                .raw(header.sender().map(|s| s.as_str()), mime, bytes.len())
            {
                Ok(admitted) => admitted,
                Err(response) => return response,
            };
        admitted.encode(
            &self
                .api
                .publish_selection_owned(mime, bytes, header.sender().map(|sender| sender.as_str()))
                .await,
        )
    }

    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
        let _admitted = match self
            .boundary
            .cancel(header.sender().map(|s| s.as_str()), request_id)
        {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let owner = header.sender().map(ToString::to_string);
        if self
            .subscriptions
            .cancel_owned(request_id, owner.as_deref())
            .await
        {
            tracing::debug!(subscription_id = %request_id, "clipboard subscription cancelled");
            return api::success(json!({ "cancelled": request_id, "kind": "subscription" }))
                .to_string();
        }
        if self
            .api
            .cancel_operation_owned(request_id, owner.as_deref())
            .await
        {
            return api::success(json!({ "cancelled": request_id, "kind": "operation" }))
                .to_string();
        }
        api::error(
            "request-not-found",
            format!("No cancellable subscription or operation named {request_id}; an operation may already be committing"),
        )
        .to_string()
    }

    #[zbus(signal)]
    async fn event(emitter: &SignalEmitter<'_>, stream: &str, event_json: &str)
    -> zbus::Result<()>;
}

async fn emit_event(
    emitter: &SignalEmitter<'_>,
    stream: &str,
    event: &str,
    subscription_id: &str,
    body: Option<shelllist_daemon_core::wire::EventBody>,
) {
    let result = shelllist_daemon_tokio::emit_event_body(
        emitter,
        INTERFACE,
        shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        stream,
        event,
        shelllist_daemon_core::Correlation::Subscription(subscription_id),
        body.unwrap_or_else(|| shelllist_daemon_core::wire::EventBody::data(json!({}))),
    )
    .await;
    if let Err(error) = result {
        tracing::warn!(%stream, %error, "clipboard subscription event could not be emitted");
    }
}

pub async fn run(backend: crate::ringboard::RingboardBackend) -> Result<()> {
    let backend = Arc::new(backend);
    let capture = crate::capture::Controller::new(backend.clone());
    run_with_capture(backend, Arc::new(capture)).await
}

pub async fn run_with_capture(
    backend: Arc<dyn ClipboardBackend>,
    capture: Arc<dyn crate::capture::CaptureControl>,
) -> Result<()> {
    let result = serve(Arc::new(ApiService::with_capture(backend, capture.clone()))).await;
    let shutdown = capture.shutdown().await.map_err(anyhow::Error::msg);
    result.and(shutdown)
}

async fn serve(api: Arc<ApiService>) -> Result<()> {
    let event_revision = Arc::new(AtomicU64::new(0));
    let (history_events, _) = tokio::sync::broadcast::channel(32);
    let tasks = shelllist_daemon_tokio::TaskGroup::default();
    tasks.spawn(
        "clipboard-history",
        subscription::observe_history(
            Arc::clone(&api),
            Arc::clone(&event_revision),
            history_events.clone(),
        ),
    );
    let connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .build()
        .await
        .context("start clip-daemon D-Bus service")?;
    let owners = owner::Owners::start(connection.clone(), api.clone(), &tasks).await?;
    let daemon = ClipDaemon::new(api.clone(), history_events, owners.clone());
    let subscriptions = daemon.subscriptions.clone();
    connection
        .object_server()
        .at(OBJECT_PATH, daemon)
        .await
        .context("export clip-daemon interface")?;
    // Publish the well-known name only after the object and owner monitor are
    // ready, preserving the retained client's D-Bus activation ordering.
    connection
        .request_name(BUS_NAME)
        .await
        .context("claim clip-daemon bus name")?;
    // Claim singleton ownership before any collector can open admission.
    api.initialize().await;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "clip-daemon started"
    );
    let result = shelllist_daemon_tokio::wait_for_shutdown().await;
    owners.shutdown(&api).await;
    let closed = connection
        .close()
        .await
        .context("close clip-daemon D-Bus service");
    subscriptions.shutdown().await;
    tasks.shutdown().await;
    result.and(closed)
}
