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
    backend::{
        BackendError, BackendErrorKind, BackendMutation, ClipboardBackend, FileSelection,
        FileSelectionOperation, HistoryQuery, MAX_QUERY_LIMIT, ScreenshotRegion,
    },
    model::{EntryDetails, EntryKind, FilePreview, OperationResult},
    session::SessionManager,
};

const MAX_EDIT_BYTES: usize = 256 * 1024;
const MAX_PUBLISHED_FILES: usize = 100;
pub type Backend = Arc<dyn ClipboardBackend>;

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

    pub(crate) fn unsupported(message: &'static str) -> Self {
        Self::new("unsupported-method", message)
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

pub(crate) struct ClipboardService {
    backend: Backend,
    edits: Mutex<HashMap<String, EditLease>>,
    sessions: SessionManager,
}

impl ClipboardService {
    pub fn new(backend: Backend) -> Self {
        Self {
            backend,
            edits: Mutex::new(HashMap::new()),
            sessions: SessionManager::default(),
        }
    }

    pub async fn dispatch_entry(
        &self,
        method: &str,
        params: Value,
        max_entry_bytes: u64,
    ) -> Result<Value, ApiError> {
        self.edits
            .lock()
            .await
            .retain(|_, lease| lease.expires > Instant::now());
        match method {
            "clipboard.entry.action" => self.action(decode(params)?, max_entry_bytes).await,
            "clipboard.entry.edit.begin" => self.begin_edit(decode(params)?).await,
            "clipboard.entry.edit.commit" => self.commit_edit(decode(params)?).await,
            "clipboard.entry.edit.cancel" => self.cancel_edit(decode(params)?).await,
            _ => Err(ApiError::unsupported("Unsupported entry method")),
        }
    }

    pub async fn dispatch_session(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        let params = || decode::<SessionParams>(params);
        match method {
            "clipboard.session.begin" => Ok(json!({ "session": self.sessions.begin().await })),
            "clipboard.session.end" => {
                Ok(json!({ "session": self.sessions.end(&params()?.session_id).await }))
            }
            "clipboard.session.hidden" => {
                let session = self
                    .sessions
                    .hidden(&params()?.session_id)
                    .await
                    .map_err(stale_target)?;
                Ok(json!({ "session": session }))
            }
            _ => Err(ApiError::unsupported("Unsupported session method")),
        }
    }

    pub(crate) async fn query(
        &self,
        params: QueryParams,
        collapse_self_echoes: bool,
    ) -> Result<Value, ApiError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&params.limit) {
            return Err(ApiError::validation("limit must be between 1 and 200"));
        }
        let history = self
            .backend
            .query(HistoryQuery {
                query: params.query,
                generation: params.generation,
                limit: params.limit,
                collapse_self_echoes,
            })
            .await?;
        Ok(json!({ "history": history }))
    }

    pub(crate) async fn details(&self, params: EntryParams) -> Result<Value, ApiError> {
        let entry = self.load_details(&params.entry_id, params.revision).await?;
        Ok(json!({ "entry": entry }))
    }

    async fn load_details(
        &self,
        entry_id: &str,
        revision: Option<u64>,
    ) -> Result<EntryDetails, ApiError> {
        validate_entry_id(entry_id)?;
        let details = self.backend.details(entry_id, MAX_EDIT_BYTES).await?;
        validate_revision(revision, details.entry.revision)?;
        Ok(details)
    }

    pub(crate) async fn thumbnail(&self, params: EntryParams) -> Result<Value, ApiError> {
        validate_entry_id(&params.entry_id)?;
        let revision = self.backend.revision(&params.entry_id).await?;
        validate_revision(params.revision, revision)?;
        let thumbnail = self
            .backend
            .thumbnail(
                &params.entry_id,
                params.revision.unwrap_or(revision),
                params.edge.unwrap_or(512),
            )
            .await?;
        Ok(json!({ "thumbnail": thumbnail }))
    }

    pub(crate) async fn capture_screenshot(
        &self,
        params: ScreenshotParams,
        max_entry_bytes: u64,
    ) -> Result<Value, ApiError> {
        if !(1..=MAX_SCREENSHOT_EDGE).contains(&params.width)
            || !(1..=MAX_SCREENSHOT_EDGE).contains(&params.height)
        {
            return Err(ApiError::validation(
                "screenshot width and height must be between 1 and 32768",
            ));
        }
        let operation = self
            .backend
            .capture_screenshot(
                ScreenshotRegion {
                    x: params.x,
                    y: params.y,
                    width: params.width,
                    height: params.height,
                },
                max_entry_bytes,
            )
            .await?;
        Ok(json!({ "operation": operation }))
    }

    pub(crate) async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max_entry_bytes: u64,
    ) -> Result<Value, ApiError> {
        if bytes.is_empty() {
            return Err(ApiError::validation("clipboard content must not be empty"));
        }
        let operation = self.backend.publish(mime, bytes, max_entry_bytes).await?;
        Ok(json!({ "operation": operation }))
    }

    pub(crate) async fn publish_files(
        &self,
        params: PublishFilesParams,
        max_entry_bytes: u64,
    ) -> Result<Value, ApiError> {
        let operation = match params.operation.as_str() {
            "copy" => FileSelectionOperation::Copy,
            "cut" => FileSelectionOperation::Cut,
            _ => return Err(ApiError::validation("operation must be copy or cut")),
        };
        if !(1..=MAX_PUBLISHED_FILES).contains(&params.paths.len()) {
            return Err(ApiError::validation(
                "paths must contain between 1 and 100 files",
            ));
        }
        if params.paths.iter().any(|path| !path.is_absolute()) {
            return Err(ApiError::validation("every file path must be absolute"));
        }
        let operation = self
            .backend
            .publish_files(
                FileSelection {
                    operation,
                    paths: params.paths,
                },
                max_entry_bytes,
            )
            .await?;
        Ok(json!({ "operation": operation }))
    }

    pub(crate) async fn change_token(&self) -> Result<u64, ApiError> {
        Ok(self.backend.change_token().await?)
    }

    pub(crate) async fn wipe(&self) -> Result<Value, ApiError> {
        Ok(json!({
            "operation": self.backend.mutate("", None, BackendMutation::Wipe).await?
        }))
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

    async fn action(&self, params: ActionParams, max_entry_bytes: u64) -> Result<Value, ApiError> {
        let details = self
            .load_details(&params.entry_id, Some(params.revision))
            .await?;
        validate_action(&details, &params.action)?;
        self.execute_action(params, details, max_entry_bytes).await
    }

    async fn execute_action(
        &self,
        params: ActionParams,
        details: EntryDetails,
        max_entry_bytes: u64,
    ) -> Result<Value, ApiError> {
        if matches!(
            params.action.as_str(),
            "open-url" | "open-file" | "reveal-file"
        ) {
            return launch_action(&params, &details).await;
        }
        if matches!(params.action.as_str(), "paste" | "image-as-file") {
            return self.paste(params, max_entry_bytes).await;
        }
        let mutation = BackendMutation::for_action(&params.action, max_entry_bytes)
            .ok_or_else(|| ApiError::validation("unsupported entry action"))?;
        let mut operation = self
            .backend
            .mutate(&params.entry_id, Some(params.revision), mutation)
            .await?;
        operation.action = params.action;
        Ok(json!({ "operation": operation }))
    }

    async fn paste(&self, params: ActionParams, max_entry_bytes: u64) -> Result<Value, ApiError> {
        let session_id = params
            .session_id
            .as_deref()
            .ok_or_else(|| ApiError::validation("session_id is required for paste"))?;
        let has_target = self
            .sessions
            .prepare_paste(session_id)
            .await
            .map_err(stale_target)?;
        let image_as_file = params.action == "image-as-file";
        let mutation = if image_as_file {
            BackendMutation::ImageAsFile {
                max_bytes: max_entry_bytes,
            }
        } else {
            BackendMutation::Restore {
                max_bytes: max_entry_bytes,
            }
        };
        let mut operation = self
            .backend
            .mutate(&params.entry_id, Some(params.revision), mutation)
            .await?;
        let feedback = match (has_target, image_as_file) {
            (true, true) => (
                "paste-prepared",
                "Image file link prepared; hide the picker",
            ),
            (true, false) => ("paste-prepared", "Paste prepared; hide the picker"),
            (false, true) => ("completed", "Image file link copied; paste manually"),
            (false, false) => ("completed", "Copied; paste manually"),
        };
        operation.status = feedback.0.into();
        operation.message = feedback.1.into();
        operation.action = params.action;
        Ok(json!({ "operation": operation }))
    }

    async fn begin_edit(&self, params: EntryParams) -> Result<Value, ApiError> {
        let details = self.load_details(&params.entry_id, params.revision).await?;
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
        let current_revision = self.backend.revision(&lease.entry_id).await?;
        validate_revision(Some(lease.revision), current_revision)?;
        let entry = self
            .backend
            .replace(
                &lease.entry_id,
                lease.revision,
                &lease.mime,
                params.value.as_bytes(),
            )
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
struct SessionParams {
    session_id: String,
}

#[derive(Deserialize)]
pub(crate) struct EntryParams {
    entry_id: String,
    revision: Option<u64>,
    edge: Option<u32>,
}

#[derive(Deserialize)]
pub(crate) struct QueryParams {
    #[serde(default)]
    query: String,
    #[serde(default)]
    generation: u64,
    #[serde(default = "default_query_limit")]
    limit: usize,
}

#[derive(Deserialize)]
pub(crate) struct PublishFilesParams {
    operation: String,
    paths: Vec<PathBuf>,
}

#[derive(Deserialize)]
pub(crate) struct ScreenshotParams {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
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

pub(crate) fn decode<T: DeserializeOwned>(params: Value) -> Result<T, ApiError> {
    serde_json::from_value(params).map_err(|error| ApiError::validation(error.to_string()))
}

fn editable_value(details: &EntryDetails) -> Result<&str, ApiError> {
    let editable = matches!(
        details.entry.kind,
        EntryKind::Text | EntryKind::Link | EntryKind::Html | EntryKind::Json | EntryKind::Color
    );
    if !editable || details.preview_truncated {
        return Err(edit_error("Clipboard entry is not safely editable"));
    }
    let value = details
        .text
        .as_deref()
        .ok_or_else(|| edit_error("Clipboard entry is not valid UTF-8 text"))?;
    (value.len() <= MAX_EDIT_BYTES)
        .then_some(value)
        .ok_or_else(|| edit_error("Clipboard entry exceeds the editable text limit"))
}

async fn launch_action(params: &ActionParams, details: &EntryDetails) -> Result<Value, ApiError> {
    let operation = match params.action.as_str() {
        "open-url" => open_url(details.text.as_deref().unwrap_or_default())?,
        "open-file" => open_file(selected_file(details, params.file_index)?)?,
        "reveal-file" => reveal_file(selected_file(details, params.file_index)?).await?,
        _ => return Err(ApiError::validation("unsupported launch action")),
    };
    Ok(json!({ "operation": operation }))
}

fn open_url(value: &str) -> Result<OperationResult, ApiError> {
    let url = Url::parse(value.trim()).map_err(|_| invalid("URL is malformed"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(invalid("Only HTTP and HTTPS URLs can be opened directly").into());
    }
    spawn("xdg-open", &[url.as_str()])?;
    Ok(OperationResult::completed("open-url", "URL opened"))
}

fn open_file(file: &FilePreview) -> Result<OperationResult, ApiError> {
    let path = local_path(file)?;
    if !path.exists() {
        return Err(BackendError::not_found("Clipboard file no longer exists").into());
    }
    let path = path.to_string_lossy();
    spawn("xdg-open", &[path.as_ref()])?;
    Ok(OperationResult::completed("open-file", "File opened"))
}

async fn reveal_file(file: &FilePreview) -> Result<OperationResult, ApiError> {
    let path = local_path(file)?;
    if !path.exists() {
        return Err(BackendError::not_found("Clipboard file no longer exists").into());
    }
    let uri = Url::from_file_path(&path)
        .map_err(|_| invalid("Clipboard entry is not a local file"))?
        .to_string();
    let connection = zbus::Connection::session()
        .await
        .map_err(|_| launch_error("Could not connect to the desktop file manager"))?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.FileManager1",
        "/org/freedesktop/FileManager1",
        "org.freedesktop.FileManager1",
    )
    .await
    .map_err(|_| launch_error("Desktop file manager is unavailable"))?;
    proxy
        .call_method("ShowItems", &(vec![uri], ""))
        .await
        .map_err(|_| launch_error("Desktop file manager could not reveal the file"))?;
    Ok(OperationResult::completed("reveal-file", "File revealed"))
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
        .map_err(|_| launch_error("Application launch failed"))
}

fn launch_error(message: &'static str) -> ApiError {
    BackendError::new(BackendErrorKind::OperationFailed, message).into()
}

fn validate_action(details: &EntryDetails, action: &str) -> Result<(), ApiError> {
    let kind = details.entry.kind;
    let allowed = match action {
        "copy" | "delete" | "favorite" | "unfavorite" | "pin-current" | "cleanup" => true,
        "paste" => kind != EntryKind::Binary,
        "image-as-file" | "annotate" => kind == EntryKind::Image,
        "open-url" => kind == EntryKind::Link,
        "open-file" | "reveal-file" => !details.files.is_empty(),
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

const MAX_SCREENSHOT_EDGE: u32 = 32_768;

const fn default_query_limit() -> usize {
    100
}
