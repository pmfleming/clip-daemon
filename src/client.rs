use anyhow::{Context, Result};
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{
    BasicCorrelation, CallFailure, CancelMode, JsonlClientConfig, run_jsonl_client,
};

use crate::{
    api,
    daemon::{BUS_NAME, INTERFACE, OBJECT_PATH},
};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("clip-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

fn call_failure(_method: &str, _error: &anyhow::Error) -> CallFailure {
    CallFailure::Api(api::error(
        "daemon-unavailable",
        "clip-daemon session service is unavailable".into(),
    ))
}

pub async fn publish(mime: &str, bytes: Vec<u8>) -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("connect to session D-Bus")?;
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .context("create clip-daemon proxy")?;
    let response: String = proxy
        .call("Publish", &(mime, bytes))
        .await
        .context("publish clipboard content")?;
    let response: Value =
        serde_json::from_str(&response).context("decode clipboard publish response")?;
    match response.get("ok").and_then(Value::as_bool) {
        Some(true) => Ok(()),
        _ => anyhow::bail!(
            response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("clip-daemon rejected the clipboard content")
                .to_owned()
        ),
    }
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: BasicCorrelation,
        cancel_mode: CancelMode::Json,
        call_failure,
        pending_event_limit: 32,
        max_in_flight_requests: 64,
        shutdown_timeout: None,
    })
    .await
}
