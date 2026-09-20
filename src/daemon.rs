use std::sync::{Arc, atomic::AtomicU64};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use shelllist_daemon_tokio::OwnedTaskRegistry;
use zbus::{connection, message::Header, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService},
    backend::ClipboardBackend,
};

mod subscription;

pub const BUS_NAME: &str = "org.laufan.ClipDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/ClipDaemon";
pub const INTERFACE: &str = "org.laufan.ClipDaemon1";

pub struct ClipDaemon {
    api: Arc<ApiService>,
    history_events: tokio::sync::broadcast::Sender<subscription::HistoryUpdate>,
    subscriptions: Arc<OwnedTaskRegistry>,
}

#[zbus::interface(name = "org.laufan.ClipDaemon1")]
impl ClipDaemon {
    async fn call(
        &self,
        method: &str,
        params_json: &str,
        #[zbus(header)] header: Header<'_>,
    ) -> String {
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(error) => {
                return api::error("validation-error", format!("invalid params JSON: {error}"))
                    .to_string();
            }
        };
        self.api
            .dispatch_owned(method, params, header.sender().map(ToString::to_string))
            .await
            .to_string()
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        let Some(owner) = header.sender().map(|sender| sender.to_owned()) else {
            return api::error(
                "subscription-unavailable",
                "D-Bus caller identity is unavailable".into(),
            )
            .to_string();
        };
        subscription::start(self, streams, owner, emitter).await
    }

    /// Publish raw bytes while keeping Wayland selection ownership in the daemon.
    async fn publish(&self, mime: &str, bytes: Vec<u8>) -> String {
        self.api.publish_selection(mime, bytes).await.to_string()
    }

    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
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
    extra: Option<Value>,
) {
    let result = shelllist_daemon_tokio::emit_json_event(
        emitter,
        INTERFACE,
        shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        stream,
        event,
        shelllist_daemon_core::Correlation::Subscription(subscription_id),
        extra.unwrap_or(Value::Null),
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
    let subscriptions = Arc::new(OwnedTaskRegistry::default());
    let daemon = ClipDaemon {
        api: api.clone(),
        history_events,
        subscriptions: Arc::clone(&subscriptions),
    };
    let connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("claim clip-daemon bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export clip-daemon interface")?
        .build()
        .await
        .context("start clip-daemon D-Bus service")?;
    // Claim singleton ownership before any collector can open admission.
    api.initialize().await;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "clip-daemon started"
    );
    let result = shelllist_daemon_tokio::wait_for_shutdown().await;
    drop(connection);
    subscriptions.shutdown().await;
    tasks.shutdown().await;
    result
}
