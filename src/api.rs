use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    backend::{ClipboardBackend, HistoryQuery},
    protocol,
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;

pub async fn dispatch(backend: Arc<dyn ClipboardBackend>, method: &str, params: Value) -> Value {
    let result = match method {
        "clipboard.history.query" => query(backend, &params).await,
        "clipboard.entry.details" => details(backend, &params).await,
        "clipboard.session.begin" => Ok(json!({ "session": {
            "id": format!("session-{}", Uuid::new_v4()),
            "target_available": false,
            "paste_mode": "copy-only",
            "expires_in_ms": 15000
        }})),
        "clipboard.settings.get" => Ok(json!({ "settings": {
            "max_entries": 750,
            "max_favorites": 100,
            "max_entry_bytes": 16777216,
            "max_editable_text_bytes": 262144,
            "capture_paused": false
        }})),
        _ if protocol::METHODS.iter().any(|item| item.0 == method) => Err((
            "not-implemented",
            format!("{method} is reserved by clip-api v1 but is not implemented yet"),
        )),
        _ => Err((
            "unsupported-method",
            format!("Unsupported clip-api method: {method}"),
        )),
    };
    match result {
        Ok(data) => success(data),
        Err((code, message)) => error(code, message),
    }
}

async fn query(
    backend: Arc<dyn ClipboardBackend>,
    params: &Value,
) -> Result<Value, (&'static str, String)> {
    let query = optional_string(params, "query")?.unwrap_or_default();
    let generation = optional_u64(params, "generation")?.unwrap_or(0);
    let limit = optional_u64(params, "limit")?.unwrap_or(100);
    let limit = usize::try_from(limit).map_err(|_| validation("limit is too large"))?;
    if !(1..=200).contains(&limit) {
        return Err(validation("limit must be between 1 and 200"));
    }
    backend
        .query(HistoryQuery {
            query,
            generation,
            limit,
        })
        .await
        .map(|history| json!({ "history": history }))
        .map_err(backend_error)
}

async fn details(
    backend: Arc<dyn ClipboardBackend>,
    params: &Value,
) -> Result<Value, (&'static str, String)> {
    let id = require_string(params, "entry_id")?;
    backend
        .details(&id, 256 * 1024)
        .await
        .map(|entry| json!({ "entry": entry }))
        .map_err(backend_error)
}

fn optional_string(params: &Value, name: &str) -> Result<Option<String>, (&'static str, String)> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(validation(&format!("'{name}' must be a string"))),
    }
}

fn require_string(params: &Value, name: &str) -> Result<String, (&'static str, String)> {
    optional_string(params, name)?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| validation(&format!("'{name}' is required")))
}

fn optional_u64(params: &Value, name: &str) -> Result<Option<u64>, (&'static str, String)> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| validation(&format!("'{name}' must be an unsigned integer"))),
    }
}

fn validation(message: &str) -> (&'static str, String) {
    ("validation-error", message.to_owned())
}
fn backend_error(error: anyhow::Error) -> (&'static str, String) {
    ("backend-unavailable", format!("{error:#}"))
}

pub fn success(data: Value) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": true, "data": data })
}

pub fn error(code: &str, message: String) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": false, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::dispatch;
    use crate::fake::FakeBackend;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn validation_and_reserved_methods_have_stable_errors() {
        let backend = Arc::new(FakeBackend::default());
        assert_eq!(
            dispatch(
                backend.clone(),
                "clipboard.history.query",
                json!({"limit": 0})
            )
            .await["error"]["code"],
            "validation-error"
        );
        assert_eq!(
            dispatch(backend.clone(), "clipboard.entry.action", json!({})).await["error"]["code"],
            "not-implemented"
        );
        assert_eq!(
            dispatch(backend, "clipboard.nope", json!({})).await["error"]["code"],
            "unsupported-method"
        );
    }
}
