use anyhow::{Context, Result};
use serde_json::Value;
use shelllist_daemon_core::DaemonEndpoint;
use shelllist_daemon_tokio::{
    CallFailure, CancelMode, CorrelationPolicy, JsonlClientConfig, TrackedId, TrackedKind,
    run_jsonl_client,
};

use crate::{
    api,
    daemon::{BUS_NAME, INTERFACE, OBJECT_PATH},
};

const ENDPOINT: DaemonEndpoint =
    DaemonEndpoint::new("clip-daemon", BUS_NAME, OBJECT_PATH, INTERFACE);

#[derive(Debug, Clone, Copy)]
struct ClipCorrelation;

impl CorrelationPolicy for ClipCorrelation {
    fn response_id(&self, response: &Value) -> Option<TrackedId> {
        tracked(
            response.pointer("/data/operation/id"),
            TrackedKind::Operation,
        )
        .or_else(|| {
            tracked(
                response.pointer("/data/subscription/id"),
                TrackedKind::Subscription,
            )
        })
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

fn tracked(value: Option<&Value>, kind: TrackedKind) -> Option<TrackedId> {
    value.and_then(Value::as_str).map(|id| TrackedId {
        id: id.to_owned(),
        kind,
    })
}

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

pub async fn screenshot(screen: bool, annotate: bool) -> Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = zbus::Proxy::new(&connection, BUS_NAME, OBJECT_PATH, INTERFACE).await?;
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
    Ok(())
}

pub async fn run() -> Result<()> {
    run_jsonl_client(JsonlClientConfig {
        endpoint: ENDPOINT,
        correlation: ClipCorrelation,
        cancel_mode: CancelMode::Json,
        call_failure,
        pending_event_limit: 32,
        max_in_flight_requests: 64,
        shutdown_timeout: None,
    })
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use shelllist_daemon_tokio::{CorrelationPolicy, TrackedKind};

    use super::ClipCorrelation;
    use crate::protocol;

    #[test]
    fn correlates_clipboard_operations_and_subscriptions() {
        let policy = ClipCorrelation;
        let operation = policy
            .response_id(&json!({ "data": { "operation": { "id": "operation-1" } } }))
            .unwrap();
        assert_eq!(operation.kind, TrackedKind::Operation);
        assert_eq!(
            policy.event_id(
                protocol::stream::OPERATION,
                &json!({ "data": { "operation": { "id": "operation-1" } } })
            ),
            Some("operation-1".into())
        );
        assert!(policy.is_terminal(
            protocol::stream::OPERATION,
            &json!({ "event": "cancelled" })
        ));

        let subscription = policy
            .response_id(&json!({ "data": { "subscription": { "id": "sub-1" } } }))
            .unwrap();
        assert_eq!(subscription.kind, TrackedKind::Subscription);
    }
}
