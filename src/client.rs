use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::Value;
use shelllist_daemon_core::{DaemonEndpoint, MAX_JSONL_REQUEST_BYTES};
use shelllist_daemon_tokio::{
    CorrelationPolicy, JsonlClientConfig, run_jsonl_client_with_input_limit,
};

use crate::daemon::{BUS_NAME, INTERFACE, OBJECT_PATH};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("clip-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

#[derive(Debug, Clone, Copy)]
struct ClipCorrelation;

impl CorrelationPolicy for ClipCorrelation {
    fn operation_id<'a>(&self, response: &'a Value) -> Option<&'a str> {
        response.pointer("/data/operation/id")?.as_str()
    }

    fn event_id(&self, stream: &str, event: &Value) -> Option<String> {
        if stream == crate::protocol::stream::OPERATION {
            event.pointer("/data/operation/id")
        } else {
            event.get("subscription_id")
        }
        .and_then(Value::as_str)
        .map(str::to_owned)
    }

    fn is_terminal(&self, stream: &str, event: &Value) -> bool {
        stream == crate::protocol::stream::OPERATION
            && matches!(
                event.get("event").and_then(Value::as_str),
                Some("completed" | "failed" | "cancelled")
            )
    }
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

pub async fn screenshot(screen: bool, annotate: bool) -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
    let mut events = proxy.receive_signal("Event").await?;
    let subscribed: String = proxy
        .call("Subscribe", &(vec![crate::protocol::stream::OPERATION],))
        .await?;
    let subscribed: Value = serde_json::from_str(&subscribed)?;
    anyhow::ensure!(
        subscribed["ok"] == true,
        "Could not observe screenshot completion"
    );
    let subscription = subscribed["data"]["subscription"]["id"]
        .as_str()
        .context("Missing subscription identity")?;
    // Ensure the receiver exists before launching. Keep this connection alive
    // throughout the interactive effect; CLI success now means a terminal result,
    // not merely acceptance followed by owner-loss cancellation.
    loop {
        let message = events
            .next()
            .await
            .context("Screenshot observation ended")?;
        let (_, event): (String, String) = message.body().deserialize()?;
        let event: Value = serde_json::from_str(&event)?;
        if event["subscription_id"] == subscription && event["event"] == "subscribed" {
            break;
        }
    }
    let params = serde_json::json!({
        "mode": if screen { "screen" } else { "region" }, "annotate": annotate,
    })
    .to_string();
    let response: String = proxy
        .call("Call", &("clipboard.capture.interactive", params))
        .await?;
    let response: Value = serde_json::from_str(&response)?;
    anyhow::ensure!(
        response["ok"] == true,
        "{}",
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .unwrap_or("Screenshot request failed")
    );
    if response["data"]["operation"]["status"] == "started" {
        let id = response["data"]["operation"]["id"]
            .as_str()
            .context("Missing screenshot operation identity")?;
        loop {
            let message = events
                .next()
                .await
                .context("Screenshot outcome unknown; do not replay")?;
            let (_, event): (String, String) = message.body().deserialize()?;
            let event: Value = serde_json::from_str(&event)?;
            if event["subscription_id"] != subscription {
                continue;
            }
            anyhow::ensure!(
                event["event"] != "gap",
                "Screenshot outcome unknown; do not replay"
            );
            if event["data"]["operation"]["id"] != id || event["event"] == "started" {
                continue;
            }
            anyhow::ensure!(
                event["event"] == "completed",
                "Screenshot did not complete: {}",
                event["event"].as_str().unwrap_or("unknown")
            );
            break;
        }
    }
    Ok(())
}

pub async fn run() -> Result<()> {
    // Text publication supports large selections, and JSON escaping can expand
    // the default 16 MiB selection sixfold. Do not apply the small RPC default.
    run_jsonl_client_with_input_limit(
        JsonlClientConfig {
            endpoint: ENDPOINT,
            correlation: ClipCorrelation,
            pending_event_limit: 32,
            max_in_flight_requests: 64,
            shutdown_timeout: None,
        },
        MAX_JSONL_REQUEST_BYTES,
    )
    .await
}
