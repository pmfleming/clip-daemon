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
    actions::{
        ActionService, ApiError, MAX_EDIT_BYTES, decode, load_details, validate_entry_id,
        validate_revision,
    },
    backend::{BackendMutation, ClipboardBackend, HistoryQuery},
    protocol,
    session::SessionManager,
    settings::{SettingsManager, SettingsUpdate},
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
const MAX_QUERY_LIMIT: usize = 200;
pub struct ApiService {
    backend: Arc<dyn ClipboardBackend>,
    sessions: SessionManager,
    wipe_challenges: Mutex<HashMap<String, Instant>>,
    settings: SettingsManager,
    actions: ActionService,
}

impl ApiService {
    pub fn new(backend: Arc<dyn ClipboardBackend>) -> Self {
        let actions = ActionService::new(Arc::clone(&backend));
        Self {
            backend,
            sessions: SessionManager::default(),
            wipe_challenges: Mutex::new(HashMap::new()),
            settings: SettingsManager::default(),
            actions,
        }
    }

    pub(crate) fn backend(&self) -> Arc<dyn ClipboardBackend> {
        Arc::clone(&self.backend)
    }

    pub async fn cancel_operation(&self, operation_id: &str) -> bool {
        self.actions.cancel(operation_id).await
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
            value if value.starts_with("clipboard.entry.") => {
                self.actions.dispatch(&self.sessions, value, params).await
            }
            value if value.starts_with("clipboard.session.") => {
                self.dispatch_session(value, params).await
            }
            value if value.starts_with("clipboard.history.wipe.") => {
                self.dispatch_wipe(value, params).await
            }
            _ => self.dispatch_policy(method, params).await,
        }
    }

    async fn dispatch_session(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.session.begin" => Ok(json!({ "session": self.sessions.begin().await })),
            "clipboard.session.end" => self.end_session(decode(params)?).await,
            "clipboard.session.hidden" => self.hide_session(decode(params)?).await,
            _ => Err(ApiError::new(
                "unsupported-method",
                "Unsupported session method",
            )),
        }
    }

    async fn dispatch_wipe(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.history.wipe.prepare" => self.prepare_wipe().await,
            "clipboard.history.wipe.commit" => self.commit_wipe(decode(params)?).await,
            _ => Err(ApiError::new(
                "unsupported-method",
                "Unsupported wipe method",
            )),
        }
    }

    async fn dispatch_policy(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.capture.setPaused" => self.set_paused(decode(params)?).await,
            "clipboard.settings.get" => self.get_settings(),
            "clipboard.settings.update" => self.update_settings(decode(params)?).await,
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
        let entry = load_details(
            &self.backend,
            &params.entry_id,
            params.revision,
            MAX_EDIT_BYTES,
        )
        .await?;
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

    fn get_settings(&self) -> Result<Value, ApiError> {
        let settings = self.settings.get().map_err(settings_error)?;
        Ok(json!({ "settings": settings }))
    }

    async fn update_settings(&self, update: SettingsUpdate) -> Result<Value, ApiError> {
        let settings = self.settings.update(update).await.map_err(settings_error)?;
        Ok(json!({ "settings": settings }))
    }

    async fn set_paused(&self, params: PauseParams) -> Result<Value, ApiError> {
        let settings = self
            .settings
            .set_paused(params.paused, params.private_mode)
            .await
            .map_err(settings_error)?;
        Ok(json!({ "capture": {
            "paused": settings.capture_paused,
            "private_mode": settings.private_mode
        }, "settings": settings }))
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
        self.actions.clear().await;
        Ok(json!({ "operation": self.backend.mutate("", BackendMutation::Wipe).await? }))
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
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
struct WipeParams {
    challenge_id: String,
    response: String,
}

#[derive(Deserialize)]
struct PauseParams {
    paused: bool,
    #[serde(default)]
    private_mode: bool,
}

fn session_error(message: &'static str) -> ApiError {
    ApiError::new("stale-target", message)
}

fn settings_error(message: String) -> ApiError {
    ApiError::new("settings-error", message)
}

const fn default_query_limit() -> usize {
    100
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
