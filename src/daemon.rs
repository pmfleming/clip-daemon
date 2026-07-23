use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::{Value, json};
use zbus::{connection, object_server::SignalEmitter};

use crate::{api, backend::ClipboardBackend, protocol};

pub const BUS_NAME: &str = "org.laufan.ClipDaemon";
pub const OBJECT_PATH: &str = "/org/laufan/ClipDaemon";
pub const INTERFACE: &str = "org.laufan.ClipDaemon1";

pub struct ClipDaemon {
    backend: Arc<dyn ClipboardBackend>,
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
        api::dispatch(Arc::clone(&self.backend), method, params)
            .await
            .to_string()
    }

    async fn subscribe(
        &self,
        streams: Vec<String>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> String {
        if let Some(stream) = streams.iter().find(|stream| {
            !protocol::STREAMS
                .iter()
                .any(|known| known.0 == stream.as_str())
        }) {
            return api::error("validation-error", format!("unknown stream: {stream}")).to_string();
        }
        let subscription_id = format!("subscription-{}", uuid::Uuid::new_v4());
        for stream in &streams {
            let event = json!({
                "protocol": api::PROTOCOL, "version": api::VERSION,
                "stream": stream, "event": "subscribed", "subscription_id": subscription_id
            });
            if let Err(error) = Self::event(&emitter, stream, &event.to_string()).await {
                return api::error("subscription-unavailable", error.to_string()).to_string();
            }
        }
        api::success(json!({ "subscription_id": subscription_id, "streams": streams })).to_string()
    }

    async fn cancel(&self, request_id: &str) -> String {
        api::success(json!({ "cancelled": request_id })).to_string()
    }

    #[zbus(signal)]
    async fn event(emitter: &SignalEmitter<'_>, stream: &str, event_json: &str)
    -> zbus::Result<()>;
}

pub async fn run(backend: Arc<dyn ClipboardBackend>) -> Result<()> {
    let _connection = connection::Builder::session()
        .context("connect to session D-Bus")?
        .name(BUS_NAME)
        .context("claim clip-daemon bus name")?
        .serve_at(OBJECT_PATH, ClipDaemon { backend })
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
