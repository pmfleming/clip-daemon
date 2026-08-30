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

impl ClipDaemon {
    fn next_id(&self, prefix: &str) -> String {
        self.subscriptions.next_id(prefix)
    }
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
            format!("No active subscription or operation named {request_id}"),
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
    let envelope = shelllist_daemon_core::event_envelope(
        shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        stream,
        event,
        shelllist_daemon_core::Correlation::Subscription(subscription_id),
        extra.unwrap_or(Value::Null),
    );
    if let Err(error) = ClipDaemon::event(emitter, stream, &envelope.to_string()).await {
        tracing::warn!(%stream, %error, "clipboard subscription event could not be emitted");
    }
}

pub async fn run(backend: Arc<dyn ClipboardBackend>) -> Result<()> {
    let api = Arc::new(ApiService::new(backend));
    let event_revision = Arc::new(AtomicU64::new(0));
    let (history_events, _) = tokio::sync::broadcast::channel(32);
    tokio::spawn(subscription::observe_history(
        Arc::clone(&api),
        Arc::clone(&event_revision),
        history_events.clone(),
    ));
    let daemon = ClipDaemon {
        api,
        history_events,
        subscriptions: Arc::new(OwnedTaskRegistry::default()),
    };
    let _connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("claim clip-daemon bus name")?
        .serve_at(OBJECT_PATH, daemon)
        .context("export clip-daemon interface")?
        .build()
        .await
        .context("start clip-daemon D-Bus service")?;
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "clip-daemon started"
    );
    shelllist_daemon_tokio::wait_for_shutdown().await
}
