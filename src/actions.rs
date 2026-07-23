use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{process::Command, sync::Mutex};
use url::Url;
use uuid::Uuid;

use crate::{
    backend::{BackendError, BackendErrorKind, BackendMutation, ClipboardBackend},
    model::{EntryDetails, EntryKind, FilePreview},
    session::SessionManager,
};

pub(crate) const MAX_EDIT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct ApiError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }

    pub(crate) fn validation(message: impl Into<String>) -> Self {
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

pub(crate) struct ActionService {
    backend: Arc<dyn ClipboardBackend>,
    edits: Mutex<HashMap<String, EditLease>>,
}

impl ActionService {
    pub fn new(backend: Arc<dyn ClipboardBackend>) -> Self {
        Self {
            backend,
            edits: Mutex::new(HashMap::new()),
        }
    }

    pub async fn dispatch(
        &self,
        sessions: &SessionManager,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        match method {
            "clipboard.entry.action" => self.action(sessions, decode(params)?).await,
            "clipboard.entry.edit.begin" => self.begin_edit(decode(params)?).await,
            "clipboard.entry.edit.commit" => self.commit_edit(decode(params)?).await,
            "clipboard.entry.edit.cancel" => self.cancel_edit(decode(params)?).await,
            _ => Err(ApiError::new(
                "unsupported-method",
                "Unsupported entry method",
            )),
        }
    }

    pub async fn cancel(&self, operation_id: &str) -> bool {
        self.backend
            .cancel_operation(operation_id)
            .await
            .unwrap_or(false)
    }

    pub async fn clear(&self) {
        self.edits.lock().await.clear();
    }

    async fn action(
        &self,
        sessions: &SessionManager,
        params: ActionParams,
    ) -> Result<Value, ApiError> {
        let details = load_details(
            &self.backend,
            &params.entry_id,
            Some(params.revision),
            MAX_EDIT_BYTES,
        )
        .await?;
        validate_action_kind(details.entry.kind, &params.action)?;
        self.execute_action(sessions, params, details).await
    }

    async fn execute_action(
        &self,
        sessions: &SessionManager,
        params: ActionParams,
        details: EntryDetails,
    ) -> Result<Value, ApiError> {
        if is_launch(&params.action) {
            return launch_action(&params, &details).await;
        }
        let target = if params.action == "paste" {
            let id = params
                .session_id
                .as_deref()
                .ok_or_else(|| ApiError::validation("session_id is required for paste"))?;
            Some(sessions.prepare_paste(id).await.map_err(stale_target)?)
        } else {
            None
        };
        let mutation = BackendMutation::for_action(&params.action)
            .ok_or_else(|| ApiError::validation("unsupported entry action"))?;
        let mut operation = self.backend.mutate(&params.entry_id, mutation).await?;
        if let Some(target) = target {
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
        }
        operation.action = params.action;
        Ok(json!({ "operation": operation }))
    }

    async fn begin_edit(&self, params: EntryParams) -> Result<Value, ApiError> {
        let details = load_details(
            &self.backend,
            &params.entry_id,
            params.revision,
            MAX_EDIT_BYTES,
        )
        .await?;
        let value = editable_value(&details)?;
        let id = format!("edit-{}", Uuid::new_v4());
        let view = json!({
            "id": id, "entry_id": details.entry.id, "revision": details.entry.revision,
            "mime": details.entry.mime, "value": value, "max_bytes": MAX_EDIT_BYTES,
            "expires_in_ms": 60000
        });
        self.edits.lock().await.insert(
            id,
            EditLease {
                entry_id: details.entry.id,
                revision: details.entry.revision,
                mime: details.entry.mime,
                expires: Instant::now() + Duration::from_secs(60),
            },
        );
        Ok(json!({ "edit": view }))
    }

    async fn commit_edit(&self, params: EditCommitParams) -> Result<Value, ApiError> {
        let lease = self
            .edits
            .lock()
            .await
            .remove(&params.edit_id)
            .ok_or_else(|| edit_error("Edit session is unknown or already used"))?;
        if lease.expires <= Instant::now() {
            return Err(edit_error("Edit session expired"));
        }
        if params.value.len() > MAX_EDIT_BYTES {
            return Err(edit_error("Edited text exceeds the configured limit"));
        }
        let current = self.backend.details(&lease.entry_id, 0).await?;
        validate_revision(Some(lease.revision), current.entry.revision)?;
        let entry = self
            .backend
            .replace(&lease.entry_id, &lease.mime, params.value.as_bytes())
            .await?;
        Ok(json!({ "entry": entry }))
    }

    async fn cancel_edit(&self, params: EditCancelParams) -> Result<Value, ApiError> {
        let cancelled = self.edits.lock().await.remove(&params.edit_id).is_some();
        Ok(json!({ "edit": { "id": params.edit_id, "cancelled": cancelled } }))
    }
}

struct EditLease {
    entry_id: String,
    revision: u64,
    mime: String,
    expires: Instant,
}

#[derive(Deserialize)]
struct ActionParams {
    entry_id: String,
    revision: u64,
    action: String,
    session_id: Option<String>,
    file_index: Option<usize>,
}

#[derive(Deserialize)]
struct EntryParams {
    entry_id: String,
    revision: Option<u64>,
}

#[derive(Deserialize)]
struct EditCommitParams {
    edit_id: String,
    value: String,
}

#[derive(Deserialize)]
struct EditCancelParams {
    edit_id: String,
}

pub(crate) async fn load_details(
    backend: &Arc<dyn ClipboardBackend>,
    entry_id: &str,
    revision: Option<u64>,
    max_bytes: usize,
) -> Result<EntryDetails, ApiError> {
    validate_entry_id(entry_id)?;
    let details = backend.details(entry_id, max_bytes).await?;
    validate_revision(revision, details.entry.revision)?;
    Ok(details)
}

pub(crate) fn decode<T: DeserializeOwned>(params: Value) -> Result<T, ApiError> {
    serde_json::from_value(params).map_err(|error| ApiError::validation(error.to_string()))
}

fn editable_value(details: &EntryDetails) -> Result<String, ApiError> {
    let editable = matches!(
        details.entry.kind,
        EntryKind::Text | EntryKind::Link | EntryKind::Html | EntryKind::Json | EntryKind::Color
    );
    if !editable || details.preview_truncated {
        return Err(edit_error("Clipboard entry is not safely editable"));
    }
    let value = details
        .text
        .clone()
        .ok_or_else(|| edit_error("Clipboard entry is not valid UTF-8 text"))?;
    (value.len() <= MAX_EDIT_BYTES)
        .then_some(value)
        .ok_or_else(|| edit_error("Clipboard entry exceeds the editable text limit"))
}

async fn launch_action(params: &ActionParams, details: &EntryDetails) -> Result<Value, ApiError> {
    let operation = match params.action.as_str() {
        "open-url" => open_url(details.text.as_deref().unwrap_or_default())?,
        "open-file" => open_file(selected_file(details, params.file_index)?, false)?,
        "reveal-file" => open_file(selected_file(details, params.file_index)?, true)?,
        _ => return Err(ApiError::validation("unsupported launch action")),
    };
    Ok(json!({ "operation": operation }))
}

fn open_url(value: &str) -> Result<crate::model::OperationResult, ApiError> {
    let url = Url::parse(value.trim()).map_err(|_| invalid("URL is malformed"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("Only HTTP and HTTPS URLs can be opened directly").into());
    }
    spawn("xdg-open", &[url.as_str()])?;
    Ok(crate::model::OperationResult::completed(
        "open-url",
        "URL opened",
    ))
}

fn open_file(file: &FilePreview, reveal: bool) -> Result<crate::model::OperationResult, ApiError> {
    let path = local_path(file)?;
    if !path.exists() {
        return Err(BackendError::not_found("Clipboard file no longer exists").into());
    }
    let path = path.to_string_lossy();
    let (program, arguments, action, message) = if reveal {
        (
            "dolphin",
            vec!["--select", path.as_ref()],
            "reveal-file",
            "File revealed",
        )
    } else {
        ("xdg-open", vec![path.as_ref()], "open-file", "File opened")
    };
    spawn(program, &arguments)?;
    Ok(crate::model::OperationResult::completed(action, message))
}

fn local_path(file: &FilePreview) -> Result<PathBuf, ApiError> {
    Url::parse(&file.uri)
        .ok()
        .and_then(|url| url.to_file_path().ok())
        .ok_or_else(|| invalid("Clipboard entry is not a local file").into())
}

fn spawn(program: &str, arguments: &[&str]) -> Result<(), ApiError> {
    Command::new(program)
        .args(arguments)
        .spawn()
        .map(|_| ())
        .map_err(|_| {
            BackendError::new(
                BackendErrorKind::OperationFailed,
                "Application launch failed",
            )
            .into()
        })
}

fn is_launch(action: &str) -> bool {
    matches!(action, "open-url" | "open-file" | "reveal-file")
}

fn validate_action_kind(kind: EntryKind, action: &str) -> Result<(), ApiError> {
    let allowed = match action {
        "copy" | "delete" | "favorite" | "unfavorite" | "pin-current" | "cleanup" => true,
        "paste" => kind != EntryKind::Binary,
        "image-as-file" | "annotate" => kind == EntryKind::Image,
        "open-url" => kind == EntryKind::Link,
        "open-file" | "reveal-file" => kind == EntryKind::Files,
        _ => false,
    };
    allowed
        .then_some(())
        .ok_or_else(|| ApiError::validation("action is unsafe for this clipboard type"))
}

fn selected_file(details: &EntryDetails, index: Option<usize>) -> Result<&FilePreview, ApiError> {
    details
        .files
        .get(index.unwrap_or_default())
        .ok_or_else(|| ApiError::validation("file_index does not identify a clipboard file"))
}

pub(crate) fn validate_entry_id(id: &str) -> Result<(), ApiError> {
    (!id.is_empty())
        .then_some(())
        .ok_or_else(|| ApiError::validation("entry_id is required"))
}

pub(crate) fn validate_revision(expected: Option<u64>, actual: u64) -> Result<(), ApiError> {
    match expected {
        Some(revision) if revision != actual => Err(ApiError {
            code: "stale-entry".into(),
            message: "Clipboard entry changed; refresh and try again".into(),
            retryable: true,
        }),
        _ => Ok(()),
    }
}

fn edit_error(message: &'static str) -> ApiError {
    ApiError::new("edit-error", message)
}

fn stale_target(message: &'static str) -> ApiError {
    ApiError::new("stale-target", message)
}

fn invalid(message: &'static str) -> BackendError {
    BackendError::new(BackendErrorKind::InvalidData, message)
}
