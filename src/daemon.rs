use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use tokio::{sync::Mutex, task::JoinHandle};
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
    sequence: AtomicU64,
    event_revision: Arc<AtomicU64>,
    subscriptions: Arc<Mutex<HashMap<String, JoinHandle<()>>>>,
}

impl ClipDaemon {
    fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.sequence.fetch_add(1, Ordering::Relaxed))
    }
}

#[zbus::interface(name = "org.laufan.ClipDaemon1")]
impl ClipDaemon {
    async fn call(&self, method: &str, params_json: &str) -> String {
        let params: Value = match serde_json::from_str(params_json) {
            Ok(value) => value,
            Err(error) => {
                return api::error("validation-error", format!("invalid params JSON: {error}"))
                    .to_string();
            }
        };
        self.api.dispatch(method, params).await.to_string()
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

    async fn cancel(&self, request_id: &str) -> String {
        if let Some(task) = self.subscriptions.lock().await.remove(request_id) {
            task.abort();
            tracing::debug!(subscription_id = %request_id, "clipboard subscription cancelled");
            return api::success(json!({ "cancelled": request_id, "kind": "subscription" }))
                .to_string();
        }
        api::error(
            "request-not-found",
            format!("No active subscription named {request_id}"),
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
    let mut envelope = Map::from_iter([
        ("protocol".into(), json!(api::PROTOCOL)),
        ("version".into(), json!(api::VERSION)),
        ("stream".into(), json!(stream)),
        ("event".into(), json!(event)),
        ("subscription_id".into(), json!(subscription_id)),
    ]);
    if let Some(Value::Object(fields)) = extra {
        envelope.extend(fields);
    }
    if let Err(error) =
        ClipDaemon::event(emitter, stream, &Value::Object(envelope).to_string()).await
    {
        tracing::warn!(%stream, %error, "clipboard subscription event could not be emitted");
    }
}

pub async fn run(backend: Arc<dyn ClipboardBackend>) -> Result<()> {
    let daemon = ClipDaemon {
        api: Arc::new(ApiService::new(backend)),
        sequence: AtomicU64::new(1),
        event_revision: Arc::new(AtomicU64::new(0)),
        subscriptions: Arc::new(Mutex::new(HashMap::new())),
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
    tokio::signal::ctrl_c()
        .await
        .context("wait for shutdown signal")
}
