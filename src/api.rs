use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    backend::{BackendError, ClipboardBackend, HistoryQuery},
    protocol,
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
const MAX_QUERY_LIMIT: usize = 200;
const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct ApiError {
    code: String,
    message: String,
    retryable: bool,
}

impl ApiError {
    fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    fn validation(message: impl Into<String>) -> Self {
        Self::new("validation-error", message)
    }
}

impl From<BackendError> for ApiError {
    fn from(error: BackendError) -> Self {
        Self {
            code: error.kind.code().into(),
            message: error.to_string(),
            retryable: error.kind.retryable(),
        }
    }
}

#[derive(Deserialize)]
struct QueryParams {
    #[serde(default)]
    query: String,
    #[serde(default)]
    generation: u64,
    #[serde(default = "default_query_limit")]
    limit: usize,
}

#[derive(Deserialize)]
struct EntryParams {
    entry_id: String,
    revision: Option<u64>,
    edge: Option<u32>,
}

pub async fn dispatch(backend: Arc<dyn ClipboardBackend>, method: &str, params: Value) -> Value {
    tracing::debug!(%method, "clip-api request started");
    match dispatch_method(backend, method, params).await {
        Ok(data) => success(data),
        Err(error) => {
            tracing::warn!(%method, code = %error.code, "clip-api request failed");
            error_response(error)
        }
    }
}

async fn dispatch_method(
    backend: Arc<dyn ClipboardBackend>,
    method: &str,
    params: Value,
) -> Result<Value, ApiError> {
    match method {
        "clipboard.history.query" => query(&backend, decode(params)?).await,
        "clipboard.entry.details" => details(&backend, decode(params)?).await,
        "clipboard.entry.thumbnail" => thumbnail(&backend, decode(params)?).await,
        "clipboard.session.begin" => Ok(session()),
        "clipboard.settings.get" => Ok(settings()),
        _ if protocol::METHODS.contains(&method) => Err(ApiError::new(
            "not-implemented",
            format!("{method} is reserved by clip-api v1 but is not implemented yet"),
        )),
        _ => Err(ApiError::new(
            "unsupported-method",
            format!("Unsupported clip-api method: {method}"),
        )),
    }
}

async fn query(
    backend: &Arc<dyn ClipboardBackend>,
    params: QueryParams,
) -> Result<Value, ApiError> {
    if !(1..=MAX_QUERY_LIMIT).contains(&params.limit) {
        return Err(ApiError::validation("limit must be between 1 and 200"));
    }
    let history = backend
        .query(HistoryQuery {
            query: params.query,
            generation: params.generation,
            limit: params.limit,
        })
        .await?;
    Ok(json!({ "history": history }))
}

async fn details(
    backend: &Arc<dyn ClipboardBackend>,
    params: EntryParams,
) -> Result<Value, ApiError> {
    validate_entry_id(&params.entry_id)?;
    let entry = backend.details(&params.entry_id, MAX_TEXT_BYTES).await?;
    validate_revision(params.revision, entry.entry.revision)?;
    Ok(json!({ "entry": entry }))
}

async fn thumbnail(
    backend: &Arc<dyn ClipboardBackend>,
    params: EntryParams,
) -> Result<Value, ApiError> {
    validate_entry_id(&params.entry_id)?;
    let thumbnail = backend
        .thumbnail(&params.entry_id, params.edge.unwrap_or(512))
        .await?;
    validate_revision(params.revision, thumbnail.revision)?;
    Ok(json!({ "thumbnail": thumbnail }))
}

fn decode<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, ApiError> {
    serde_json::from_value(params).map_err(|error| ApiError::validation(error.to_string()))
}

fn validate_entry_id(id: &str) -> Result<(), ApiError> {
    (!id.is_empty())
        .then_some(())
        .ok_or_else(|| ApiError::validation("entry_id is required"))
}

fn validate_revision(expected: Option<u64>, actual: u64) -> Result<(), ApiError> {
    match expected {
        Some(revision) if revision != actual => Err(ApiError {
            code: "stale-entry".into(),
            message: "Clipboard entry changed; refresh and try again".into(),
            retryable: true,
        }),
        _ => Ok(()),
    }
}

const fn default_query_limit() -> usize {
    100
}

fn session() -> Value {
    json!({ "session": {
        "id": format!("session-{}", Uuid::new_v4()),
        "target_available": false,
        "paste_mode": "copy-only",
        "expires_in_ms": 15000
    }})
}

fn settings() -> Value {
    json!({ "settings": {
        "max_entries": 750,
        "max_favorites": 100,
        "max_entry_bytes": 16777216,
        "max_editable_text_bytes": MAX_TEXT_BYTES,
        "capture_paused": false
    }})
}

pub fn success(data: Value) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": true, "data": data })
}

fn error_response(error: ApiError) -> Value {
    json!({
        "protocol": PROTOCOL, "version": VERSION, "ok": false,
        "error": { "code": error.code, "message": error.message, "retryable": error.retryable }
    })
}

pub fn error(code: &str, message: String) -> Value {
    error_response(ApiError::new(code, message))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use crate::fake::FakeBackend;

    use super::dispatch;

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
