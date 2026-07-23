use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    backend::{BackendError, ClipboardBackend, HistoryQuery},
    protocol,
    session::SessionManager,
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
const MAX_QUERY_LIMIT: usize = 200;
const MAX_TEXT_BYTES: usize = 256 * 1024;

pub struct ApiService {
    backend: Arc<dyn ClipboardBackend>,
    sessions: SessionManager,
    wipe_challenges: Mutex<HashMap<String, Instant>>,
}

impl ApiService {
    pub fn new(backend: Arc<dyn ClipboardBackend>) -> Self {
        Self {
            backend,
            sessions: SessionManager::default(),
            wipe_challenges: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn backend(&self) -> Arc<dyn ClipboardBackend> {
        Arc::clone(&self.backend)
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Value {
        tracing::debug!(%method, "clip-api request started");
        match self.dispatch_method(method, params).await {
            Ok(data) => success(data),
            Err(error) => {
                tracing::warn!(%method, code = %error.code, "clip-api request failed");
                error_response(error)
            }
        }
    }

    async fn dispatch_method(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.history.query" => self.query(decode(params)?).await,
            "clipboard.entry.details" => self.details(decode(params)?).await,
            "clipboard.entry.thumbnail" => self.thumbnail(decode(params)?).await,
            "clipboard.entry.action" => self.action(decode(params)?).await,
            "clipboard.session.begin" => Ok(json!({ "session": self.sessions.begin().await })),
            "clipboard.session.end" => self.end_session(decode(params)?).await,
            "clipboard.session.hidden" => self.hide_session(decode(params)?).await,
            "clipboard.history.wipe.prepare" => self.prepare_wipe().await,
            "clipboard.history.wipe.commit" => self.commit_wipe(decode(params)?).await,
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

    async fn query(&self, params: QueryParams) -> Result<Value, ApiError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&params.limit) {
            return Err(ApiError::validation("limit must be between 1 and 200"));
        }
        let history = self
            .backend
            .query(HistoryQuery {
                query: params.query,
                generation: params.generation,
                limit: params.limit,
            })
            .await?;
        Ok(json!({ "history": history }))
    }

    async fn details(&self, params: EntryParams) -> Result<Value, ApiError> {
        validate_entry_id(&params.entry_id)?;
        let entry = self
            .backend
            .details(&params.entry_id, MAX_TEXT_BYTES)
            .await?;
        validate_revision(params.revision, entry.entry.revision)?;
        Ok(json!({ "entry": entry }))
    }

    async fn thumbnail(&self, params: EntryParams) -> Result<Value, ApiError> {
        validate_entry_id(&params.entry_id)?;
        let thumbnail = self
            .backend
            .thumbnail(&params.entry_id, params.edge.unwrap_or(512))
            .await?;
        validate_revision(params.revision, thumbnail.revision)?;
        Ok(json!({ "thumbnail": thumbnail }))
    }

    async fn action(&self, params: ActionParams) -> Result<Value, ApiError> {
        validate_entry_id(&params.entry_id)?;
        let details = self.backend.details(&params.entry_id, 0).await?;
        validate_revision(Some(params.revision), details.entry.revision)?;
        let mut operation = match params.action.as_str() {
            "copy" => self.backend.restore(&params.entry_id).await?,
            "paste" => {
                let session_id = params
                    .session_id
                    .as_deref()
                    .ok_or_else(|| ApiError::validation("session_id is required for paste"))?;
                let target = self
                    .sessions
                    .prepare_paste(session_id)
                    .await
                    .map_err(session_error)?;
                let mut operation = self.backend.restore(&params.entry_id).await?;
                operation.action = "paste".into();
                operation.status = if target {
                    "paste-prepared"
                } else {
                    "completed"
                }
                .into();
                operation.message = if target {
                    "Paste prepared; hide the picker"
                } else {
                    "Copied; paste manually"
                }
                .into();
                operation
            }
            "image-as-file" => self.backend.image_as_file(&params.entry_id).await?,
            "annotate" => self.backend.annotate(&params.entry_id).await?,
            _ => return Err(ApiError::validation("unsupported entry action")),
        };
        operation.action = params.action;
        Ok(json!({ "operation": operation }))
    }

    async fn end_session(&self, params: SessionParams) -> Result<Value, ApiError> {
        Ok(json!({ "session": self.sessions.end(&params.session_id).await }))
    }

    async fn hide_session(&self, params: SessionParams) -> Result<Value, ApiError> {
        let session = self
            .sessions
            .hidden(&params.session_id)
            .await
            .map_err(session_error)?;
        Ok(json!({ "session": session }))
    }

    async fn prepare_wipe(&self) -> Result<Value, ApiError> {
        let id = format!("challenge-{}", Uuid::new_v4());
        let expires = Instant::now() + Duration::from_secs(30);
        let mut challenges = self.wipe_challenges.lock().await;
        challenges.retain(|_, deadline| *deadline > Instant::now());
        challenges.insert(id.clone(), expires);
        Ok(json!({ "challenge": { "id": id, "confirmation": "WIPE", "expires_in_ms": 30000 } }))
    }

    async fn commit_wipe(&self, params: WipeParams) -> Result<Value, ApiError> {
        if params.response != "WIPE" {
            return Err(ApiError::validation("wipe confirmation must be WIPE"));
        }
        let deadline = self
            .wipe_challenges
            .lock()
            .await
            .remove(&params.challenge_id)
            .ok_or_else(|| {
                ApiError::new("stale-action", "wipe challenge is unknown or already used")
            })?;
        if deadline <= Instant::now() {
            return Err(ApiError::new("stale-action", "wipe challenge expired"));
        }
        Ok(json!({ "operation": self.backend.wipe().await? }))
    }
}

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

#[derive(Deserialize)]
struct ActionParams {
    entry_id: String,
    revision: u64,
    action: String,
    session_id: Option<String>,
}

#[derive(Deserialize)]
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
struct WipeParams {
    challenge_id: String,
    response: String,
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

fn session_error(message: &'static str) -> ApiError {
    ApiError::new("stale-target", message)
}

const fn default_query_limit() -> usize {
    100
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
    use super::ApiService;
    use crate::fake::FakeBackend;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn validation_reserved_methods_and_wipe_challenges_are_stable() {
        let api = ApiService::new(Arc::new(FakeBackend::default()));
        assert_eq!(
            api.dispatch("clipboard.history.query", json!({"limit": 0}))
                .await["error"]["code"],
            "validation-error"
        );
        assert_eq!(
            api.dispatch("clipboard.entry.edit.begin", json!({})).await["error"]["code"],
            "not-implemented"
        );
        assert_eq!(
            api.dispatch("clipboard.nope", json!({})).await["error"]["code"],
            "unsupported-method"
        );
        let challenge = api
            .dispatch("clipboard.history.wipe.prepare", json!({}))
            .await;
        let id = challenge["data"]["challenge"]["id"].as_str().unwrap();
        let result = api
            .dispatch(
                "clipboard.history.wipe.commit",
                json!({"challenge_id": id, "response": "WIPE"}),
            )
            .await;
        assert_eq!(result["data"]["operation"]["action"], "wipe");
    }
}
