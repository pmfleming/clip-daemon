use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{
    actions::{self, ApiError, ClipboardService, decode},
    protocol,
    settings::{SettingsManager, SettingsUpdate},
};

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
pub struct ApiService {
    wipe_challenges: Mutex<HashMap<String, Instant>>,
    settings: SettingsManager,
    actions: ClipboardService,
}

impl ApiService {
    pub fn new(backend: actions::Backend) -> Self {
        Self {
            wipe_challenges: Mutex::new(HashMap::new()),
            settings: SettingsManager::default(),
            actions: ClipboardService::new(backend),
        }
    }

    pub(crate) async fn change_token(&self) -> Result<u64, Value> {
        self.actions
            .change_token()
            .await
            .map_err(|error| json!({ "error": { "code": error.code, "message": error.message } }))
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
            "clipboard.history.query" => self.actions.query(decode(params)?).await,
            "clipboard.entry.details" => self.actions.details(decode(params)?).await,
            "clipboard.entry.thumbnail" => self.actions.thumbnail(decode(params)?).await,
            value if value.starts_with("clipboard.entry.") => {
                let max_entry_bytes = self.settings.get().map_err(settings_error)?.max_entry_bytes;
                self.actions
                    .dispatch_entry(value, params, max_entry_bytes)
                    .await
            }
            value if value.starts_with("clipboard.session.") => {
                self.actions.dispatch_session(value, params).await
            }
            value if value.starts_with("clipboard.history.wipe.") => {
                self.dispatch_wipe(value, params).await
            }
            _ => self.dispatch_policy(method, params).await,
        }
    }

    async fn dispatch_wipe(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.history.wipe.prepare" => self.prepare_wipe().await,
            "clipboard.history.wipe.commit" => self.commit_wipe(decode(params)?).await,
            _ => Err(ApiError::unsupported("Unsupported wipe method")),
        }
    }

    async fn dispatch_policy(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.capture.setPaused" => self.set_paused(decode(params)?).await,
            "clipboard.capture.screenshot" => {
                let max_entry_bytes = self.settings.get().map_err(settings_error)?.max_entry_bytes;
                self.actions
                    .capture_screenshot(decode(params)?, max_entry_bytes)
                    .await
            }
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
        self.actions.wipe().await
    }
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

fn settings_error(message: String) -> ApiError {
    ApiError::new("settings-error", message)
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
