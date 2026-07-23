use std::sync::Arc;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    backend::{BackendError, ClipboardBackend, HistoryQuery},
    protocol,
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
const MAX_QUERY_LIMIT: u64 = 200;
const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
struct ApiError {
    code: String,
    message: String,
    retryable: bool,
}

impl ApiError {
    fn validation(message: impl Into<String>) -> Self {
        Self {
            code: "validation-error".into(),
            message: message.into(),
            retryable: false,
        }
    }

    fn reserved(method: &str) -> Self {
        Self {
            code: "not-implemented".into(),
            message: format!("{method} is reserved by clip-api v1 but is not implemented yet"),
            retryable: false,
        }
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

pub async fn dispatch(backend: Arc<dyn ClipboardBackend>, method: &str, params: Value) -> Value {
    tracing::debug!(%method, "clip-api request started");
    let result = dispatch_method(backend, method, &params).await;
    match result {
        Ok(data) => {
            tracing::debug!(%method, "clip-api request completed");
            success(data)
        }
        Err(error) => {
            tracing::warn!(%method, code = error.code, "clip-api request failed");
            error_response(error)
        }
    }
}

async fn dispatch_method(
    backend: Arc<dyn ClipboardBackend>,
    method: &str,
    params: &Value,
) -> Result<Value, ApiError> {
    match method {
        "clipboard.history.query" => query(&backend, params).await,
        "clipboard.entry.details" => details(&backend, params).await,
        "clipboard.entry.thumbnail" => thumbnail(&backend, params).await,
        "clipboard.session.begin" => Ok(session()),
        "clipboard.settings.get" => Ok(settings()),
        _ if protocol::METHODS.iter().any(|item| item.0 == method) => {
            Err(ApiError::reserved(method))
        }
        _ => Err(ApiError {
            code: "unsupported-method".into(),
            message: format!("Unsupported clip-api method: {method}"),
            retryable: false,
        }),
    }
}

async fn query(backend: &Arc<dyn ClipboardBackend>, params: &Value) -> Result<Value, ApiError> {
    let limit = params.optional_u64("limit")?.unwrap_or(100);
    if !(1..=MAX_QUERY_LIMIT).contains(&limit) {
        return Err(ApiError::validation("limit must be between 1 and 200"));
    }
    let history = backend
        .query(HistoryQuery {
            query: params.optional_string("query")?.unwrap_or_default(),
            generation: params.optional_u64("generation")?.unwrap_or(0),
            limit: limit as usize,
        })
        .await?;
    Ok(json!({ "history": history }))
}

async fn details(backend: &Arc<dyn ClipboardBackend>, params: &Value) -> Result<Value, ApiError> {
    let entry = backend
        .details(params.require_string("entry_id")?, MAX_TEXT_BYTES)
        .await?;
    validate_revision(params, entry.entry.revision)?;
    Ok(json!({ "entry": entry }))
}

async fn thumbnail(backend: &Arc<dyn ClipboardBackend>, params: &Value) -> Result<Value, ApiError> {
    let edge = params.optional_u64("edge")?.unwrap_or(512);
    let edge = u32::try_from(edge).map_err(|_| ApiError::validation("edge is too large"))?;
    let thumbnail = backend
        .thumbnail(params.require_string("entry_id")?, edge)
        .await?;
    validate_revision(params, thumbnail.revision)?;
    Ok(json!({ "thumbnail": thumbnail }))
}

fn validate_revision(params: &Value, actual: u64) -> Result<(), ApiError> {
    if let Some(expected) = params.optional_u64("revision")?
        && expected != actual
    {
        return Err(ApiError {
            code: "stale-entry".into(),
            message: "Clipboard entry changed; refresh and try again".into(),
            retryable: true,
        });
    }
    Ok(())
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

trait Params {
    fn optional_string(&self, name: &str) -> Result<Option<String>, ApiError>;
    fn optional_u64(&self, name: &str) -> Result<Option<u64>, ApiError>;
    fn require_string(&self, name: &str) -> Result<&str, ApiError>;
}

impl Params for Value {
    fn optional_string(&self, name: &str) -> Result<Option<String>, ApiError> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            _ => Err(ApiError::validation(format!("'{name}' must be a string"))),
        }
    }

    fn optional_u64(&self, name: &str) -> Result<Option<u64>, ApiError> {
        match self.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                ApiError::validation(format!("'{name}' must be an unsigned integer"))
            }),
        }
    }

    fn require_string(&self, name: &str) -> Result<&str, ApiError> {
        self.get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::validation(format!("'{name}' is required")))
    }
}

pub fn success(data: Value) -> Value {
    json!({ "protocol": PROTOCOL, "version": VERSION, "ok": true, "data": data })
}

fn error_response(error: ApiError) -> Value {
    json!({
        "protocol": PROTOCOL,
        "version": VERSION,
        "ok": false,
        "error": {
            "code": error.code,
            "message": error.message,
            "retryable": error.retryable
        }
    })
}

pub fn error(code: &str, message: String) -> Value {
    error_response(ApiError {
        code: code.into(),
        message,
        retryable: false,
    })
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
