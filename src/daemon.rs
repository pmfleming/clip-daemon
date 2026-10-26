use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use shelllist_daemon_tokio::{EventEmitter, OwnedTaskRegistry, SocketSession};
use zbus::{connection, message::Header, object_server::SignalEmitter};

use crate::{
    api::{self, ApiService},
    backend::ClipboardBackend,
};

mod owner;
mod socket;
mod subscription;

pub const BUS_NAME: &str = "org.laufan.ClipDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/ClipDaemon";
pub const INTERFACE: &str = "org.laufan.ClipDaemon2";

#[derive(Clone)]
enum Caller {
    Dbus(String),
    Socket(SocketSession),
}
impl Caller {
    fn id(&self) -> &str {
        match self {
            Self::Dbus(owner) => owner,
            Self::Socket(session) => session.owner(),
        }
    }
    async fn dispatch(
        self,
        owners: owner::Owners,
        api: Arc<ApiService>,
        method: String,
        params: Value,
    ) -> Value {
        match self {
            Self::Dbus(owner) => owners.dispatch(&api, &method, params, owner).await,
            Self::Socket(session) => {
                if session.is_closed() {
                    return api::error(
                        "owner-unavailable",
                        "Clipboard caller is unavailable".into(),
                    );
                }
                let response = api
                    .dispatch_owned(&method, params, Some(session.owner().into()))
                    .await;
                // This also covers resources accepted after disconnect cleanup.
                if session.is_closed() {
                    api.release_owner(session.owner()).await;
                }
                response
            }
        }
    }
}

pub struct ClipDaemon {
    boundary: shelllist_daemon_tokio::ServiceBoundary,
    api: Arc<ApiService>,
    owners: owner::Owners,
    history_events: tokio::sync::broadcast::Sender<subscription::HistoryUpdate>,
    subscriptions: Arc<OwnedTaskRegistry>,
    _workers: shelllist_daemon_tokio::TaskGroup,
}

impl ClipDaemon {
    fn new(
        api: Arc<ApiService>,
        history_events: tokio::sync::broadcast::Sender<subscription::HistoryUpdate>,
        owners: owner::Owners,
    ) -> Self {
        let workers = shelllist_daemon_tokio::TaskGroup::default();
        let events = api.operation_events();
        let observed = api.clone();
        workers.spawn("clipboard-operations", async move {
            observed.observe_operations(events).await
        });
        workers.spawn(
            "clipboard-history",
            subscription::observe_history(api.clone(), history_events.clone()),
        );
        Self {
            _workers: workers,
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

    async fn call_owned(&self, method: &str, params_json: &str, caller: Caller) -> String {
        let admitted = match self.boundary.call(Some(caller.id()), method, params_json) {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(_) => {
                return api::error("validation-error", "Invalid params JSON".into()).to_string();
            }
        };
        let owners = self.owners.clone();
        let api = self.api.clone();
        let method = method.to_owned();
        // Keep the shared admission and late-work cleanup alive when a waiter
        // disappears. This unclassified boundary has no policy for run().
        tokio::spawn(async move {
            let response = caller.dispatch(owners, api, method, params).await;
            tokio::task::spawn_blocking(move || admitted.encode(&response)).await
        })
        .await
        .ok()
        .and_then(|result| result.ok())
        .unwrap_or_else(|| {
            api::error(
                "outcome-unknown",
                "Clipboard outcome is unknown; do not replay".into(),
            )
            .to_string()
        })
    }

    async fn subscribe_owned(
        &self,
        streams: Vec<String>,
        owner: String,
        destination: EventEmitter<'static>,
        stop: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> String {
        let admitted = match self.boundary.subscribe(Some(&owner), &streams) {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        admitted.encoded(subscription::start(self, streams, owner, destination, stop).await)
    }

    async fn cancel_owned(&self, request_id: &str, owner: Option<&str>) -> String {
        let _admitted = match self.boundary.cancel(owner, request_id) {
            Ok(admitted) => admitted,
            Err(response) => return response,
        };
        if self.subscriptions.cancel_owned(request_id, owner).await {
            return api::success(json!({"cancelled":request_id, "kind":"subscription"}))
                .to_string();
        }
        if self.api.cancel_operation_owned(request_id, owner).await {
            return api::success(json!({"cancelled":request_id, "kind":"operation"})).to_string();
        }
        api::error(
            "request-not-found",
            "No owned cancellable request; an operation may already be committing".into(),
        )
        .to_string()
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
        let Some(owner) = header.sender() else {
            return api::error(
                "owner-unavailable",
                "Clipboard caller is unavailable".into(),
            )
            .to_string();
        };
        self.call_owned(method, params_json, Caller::Dbus(owner.to_string()))
            .await
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
        let monitor = self.subscriptions.owner_monitor(emitter.connection());
        let watched = owner.to_string();
        let destination = emitter.set_destination(owner.clone().into()).to_owned();
        self.subscribe_owned(streams, owner.to_string(), destination.into(), async move {
            let _ = monitor.wait(&watched).await;
        })
        .await
    }
    /// Binary D-Bus compatibility; JSON text/file publication uses the same API.
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
                .publish_selection_owned(mime, bytes, header.sender().map(|s| s.as_str()))
                .await,
        )
    }
    async fn cancel(&self, request_id: &str, #[zbus(header)] header: Header<'_>) -> String {
        self.cancel_owned(request_id, header.sender().map(|s| s.as_str()))
            .await
    }
    #[zbus(signal)]
    async fn event(emitter: &SignalEmitter<'_>, stream: &str, event_json: &str)
    -> zbus::Result<()>;
}

async fn emit_event(
    emitter: &EventEmitter<'_>,
    stream: &str,
    event: &str,
    subscription_id: &str,
    body: Option<shelllist_daemon_core::wire::EventBody>,
) {
    let body = body.unwrap_or_else(|| shelllist_daemon_core::wire::EventBody::data(json!({})));
    let value = json!(body.envelope(
        shelllist_daemon_core::ApiIdentity::new(api::PROTOCOL, api::VERSION as u32),
        stream,
        event,
        shelllist_daemon_core::Correlation::Subscription(subscription_id),
    ));
    // Synchronous enqueue is deliberately fail-closed for socket slow readers.
    let result = match emitter {
        EventEmitter::Socket(session) => session.try_emit(stream, value),
        _ => emitter.emit(INTERFACE, stream, value).await,
    };
    if let Err(error) = result {
        tracing::warn!(%stream, %error, "clipboard subscription delivery failed");
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
    let tasks = shelllist_daemon_tokio::TaskGroup::default();
    let connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .build()
        .await
        .context("start clip-daemon D-Bus service")?;
    let owners = owner::Owners::start(connection.clone(), api.clone(), &tasks).await?;
    let daemon = ClipDaemon::new(
        api.clone(),
        tokio::sync::broadcast::channel(32).0,
        owners.clone(),
    );
    let subscriptions = daemon.subscriptions.clone();
    connection
        .object_server()
        .at(OBJECT_PATH, daemon)
        .await
        .context("export clip-daemon interface")?;
    // Preserve D-Bus activation ordering and singleton capture ownership.
    connection
        .request_name(BUS_NAME)
        .await
        .context("claim clip-daemon bus name")?;
    api.initialize().await;
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let socket_connection = connection.clone();
    let mut listener = tokio::spawn(socket::listen(socket_connection, async {
        let _ = stopped.await;
    }));
    tracing::info!(
        bus_name = BUS_NAME,
        object_path = OBJECT_PATH,
        "clip-daemon started"
    );
    let (result, ended) = tokio::select! {
        result = shelllist_daemon_tokio::wait_for_shutdown() => (result, false),
        result = &mut listener => (result.context("join clipboard socket listener").and_then(|r|r), true),
    };
    let _ = stop.send(());
    let socket_result = if ended {
        Ok(())
    } else {
        listener
            .await
            .context("join clipboard socket shutdown")
            .and_then(|r| r)
    };
    owners.shutdown(&api).await;
    let closed = connection
        .close()
        .await
        .context("close clip-daemon D-Bus service");
    subscriptions.shutdown().await;
    tasks.shutdown().await;
    result.and(socket_result).and(closed)
}
