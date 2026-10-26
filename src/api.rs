use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use shelllist_daemon_core::{ApiError as EnvelopeError, ApiIdentity, error as error_envelope};
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use crate::{
    actions::{self, ApiError, ClipboardService, decode},
    protocol,
    settings::{SettingsManager, SettingsUpdate},
};

mod operations;

pub const PROTOCOL: &str = protocol::NAME;
pub const VERSION: u8 = protocol::VERSION;
const API: ApiIdentity = ApiIdentity::new(PROTOCOL, VERSION as u32);
#[derive(Clone, Debug)]
pub(crate) struct LifecycleEvent {
    pub stream: &'static str,
    pub event: String,
    pub data: Value,
    audience: EventAudience,
}

#[derive(Clone, Debug)]
enum EventAudience {
    Everyone,
    Owner(Option<String>),
}

impl LifecycleEvent {
    pub(crate) fn visible_to(&self, owner: Option<&str>) -> bool {
        match &self.audience {
            EventAudience::Everyone => true,
            EventAudience::Owner(expected) => expected.as_deref() == owner,
        }
    }
}

struct WipeChallenge {
    owner: Option<String>,
    expires: Instant,
}

pub struct ApiService {
    wipe_challenges: Mutex<HashMap<String, WipeChallenge>>,
    settings: SettingsManager,
    actions: ClipboardService,
    lifecycle_events: broadcast::Sender<LifecycleEvent>,
    operations: Arc<std::sync::Mutex<operations::Book>>,
}

impl ApiService {
    pub fn new(backend: actions::Backend) -> Self {
        Self::with_capture(backend, Arc::new(crate::capture::Unavailable))
    }

    pub(crate) fn with_settings(backend: actions::Backend, settings: SettingsManager) -> Self {
        let (lifecycle_events, _) = broadcast::channel(64);
        Self {
            wipe_challenges: Mutex::new(HashMap::new()),
            settings,
            actions: ClipboardService::new(backend),
            lifecycle_events,
            operations: Arc::new(std::sync::Mutex::new(operations::Book::default())),
        }
    }

    /// Inject capture ownership without opening Wayland in API/unit-test constructors.
    pub fn with_capture(
        backend: actions::Backend,
        capture: Arc<dyn crate::capture::CaptureControl>,
    ) -> Self {
        Self::with_settings(backend, SettingsManager::with_capture(capture))
    }

    pub(crate) async fn initialize(&self) {
        if let Err(error) = self.settings.initialize_retention().await {
            tracing::warn!(%error, "Saved retention is not verified against the running engine");
        }
        if let Err(error) = self.settings.reconcile_capture().await {
            tracing::warn!(%error, "Capture preference could not be verified; privacy is not asserted");
        }
    }

    pub(crate) async fn change_token(&self) -> Result<u64, EnvelopeError> {
        self.actions
            .change_token()
            .await
            .map_err(|error| EnvelopeError::new(error.code, error.message))
    }

    pub(crate) fn operation_events(&self) -> actions::OperationEvents {
        self.actions.operation_events()
    }

    pub(crate) fn lifecycle_events(&self) -> broadcast::Receiver<LifecycleEvent> {
        self.lifecycle_events.subscribe()
    }

    pub async fn cancel_operation(&self, operation_id: &str) -> bool {
        self.cancel_operation_owned(operation_id, None).await
    }

    pub async fn cancel_operation_owned(&self, operation_id: &str, owner: Option<&str>) -> bool {
        self.refresh_operations();
        if !self
            .operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .records
            .get(operation_id)
            .is_some_and(|record| record.owner.as_deref() == owner && record.active())
        {
            return false;
        }
        // Never hold the ownership lock while waiting for backend cancellation.
        let cancelled = self.actions.cancel(operation_id).await;
        self.refresh_operation(operation_id);
        cancelled
    }

    fn track_operation(&self, initial: crate::model::OperationResult, owner: Option<String>) {
        let mut book = self.operations.lock().unwrap_or_else(|p| p.into_inner());
        let current = self
            .actions
            .operation_status(&initial.id)
            .ok()
            .flatten()
            .unwrap_or(initial);
        let id = current.id.clone();
        book.register(current, owner);
        self.emit_operation(&book.records[&id]);
    }

    fn emit_operation(&self, record: &operations::Record) {
        let _ = self.lifecycle_events.send(LifecycleEvent {
            stream: protocol::stream::OPERATION,
            event: record.operation.status.clone(),
            data: json!({"operation": record.operation}),
            audience: EventAudience::Owner(record.owner.clone()),
        });
    }

    fn refresh_operation(&self, id: &str) {
        let mut book = self.operations.lock().unwrap_or_else(|p| p.into_inner());
        let Some(record) = book.records.get_mut(id) else {
            return;
        };
        if !record.active() {
            return;
        }
        // A queued backend event is only an invalidation. Reading current status
        // under this lock cannot regress a terminal result to a delayed start.
        let Ok(current) = self.actions.operation_status(id) else {
            return;
        };
        let current = match current {
            Some(current) => current,
            None if self
                .actions
                .active_operations()
                .is_ok_and(|active| !active.contains(id)) =>
            {
                crate::model::OperationResult::with_id(
                    id.into(),
                    &record.operation.action,
                    "unknown",
                    "Operation outcome is unavailable; do not replay",
                )
            }
            None => return,
        };
        if record.operation != current {
            record.operation = current;
            record.revision += 1;
            record.updated = Instant::now();
            self.emit_operation(record);
        }
    }

    fn refresh_operations(&self) {
        let ids: Vec<_> = self
            .operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .records
            .keys()
            .cloned()
            .collect();
        for id in ids {
            self.refresh_operation(&id);
        }
        self.operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prune();
    }

    pub(crate) async fn observe_operations(&self, mut events: actions::OperationEvents) {
        loop {
            match events.recv().await {
                Ok(operation) => self.refresh_operation(&operation.id),
                Err(broadcast::error::RecvError::Lagged(_)) => self.refresh_operations(),
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }

    pub async fn publish_selection(&self, mime: &str, bytes: Vec<u8>) -> Value {
        self.publish_selection_owned(mime, bytes, None).await
    }

    pub(crate) async fn publish_selection_owned(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        owner: Option<&str>,
    ) -> Value {
        const METHOD: &str = "clipboard.selection.publish";
        tracing::debug!(method = METHOD, "clip-api request started");
        let result = match self.max_entry_bytes() {
            Ok(limit) => self.actions.publish(mime, bytes, limit).await,
            Err(error) => Err(error),
        };
        if let Ok(data) = &result {
            self.publish_lifecycle(METHOD, data, owner);
        }
        finish_request(METHOD, result)
    }

    pub async fn dispatch(&self, method: &str, params: Value) -> Value {
        self.dispatch_owned(method, params, None).await
    }

    pub async fn dispatch_owned(
        &self,
        method: &str,
        params: Value,
        owner: Option<String>,
    ) -> Value {
        tracing::debug!(%method, "clip-api request started");
        self.refresh_operations();
        let _reservation = if operations::may_start(method, &params) {
            match operations::Book::reserve(&self.operations, owner.as_deref()) {
                Ok(reservation) => Some(reservation),
                Err(error) => return finish_request(method, Err(error)),
            }
        } else {
            None
        };
        let result = if method == "clipboard.history.query" {
            self.query_history(params, owner.as_deref()).await
        } else {
            self.dispatch_method(method, params, owner.as_deref()).await
        };
        if let Ok(data) = &result {
            self.publish_lifecycle(method, data, owner.as_deref());
        }
        if let Some(data) = result
            .as_ref()
            .ok()
            .filter(|data| started_operation_id(data).is_some())
            && let Ok(operation) = serde_json::from_value(data["operation"].clone())
        {
            self.track_operation(operation, owner);
        }
        finish_request(method, result)
    }

    /// Revoke authority and request cancellation. Committing work retains its
    /// actual outcome; disconnect is never evidence of rollback or completion.
    pub(crate) async fn release_owner(&self, owner: &str) {
        self.actions.release_owner(owner).await;
        let ids: Vec<_> = self
            .operations
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .records
            .iter()
            .filter(|(_, record)| record.active() && record.owner.as_deref() == Some(owner))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.actions.cancel(&id).await;
            self.refresh_operation(&id);
        }
        self.wipe_challenges
            .lock()
            .await
            .retain(|_, challenge| challenge.owner.as_deref() != Some(owner));
    }

    pub(crate) async fn resource_owners(&self) -> std::collections::HashSet<String> {
        let mut owners: std::collections::HashSet<_> =
            self.actions.owners().await.into_iter().collect();
        owners.extend(
            self.operations
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .records
                .values()
                .filter(|record| record.active())
                .filter_map(|record| record.owner.clone()),
        );
        let mut challenges = self.wipe_challenges.lock().await;
        challenges.retain(|_, challenge| challenge.expires > Instant::now());
        owners.extend(
            challenges
                .values()
                .filter_map(|challenge| challenge.owner.clone()),
        );
        owners
    }

    fn publish_lifecycle(&self, method: &str, data: &Value, owner: Option<&str>) {
        for event in [
            operation_event(data, owner),
            policy_event(method, data, owner),
        ]
        .into_iter()
        .flatten()
        {
            let _ = self.lifecycle_events.send(event);
        }
    }

    async fn dispatch_method(
        &self,
        method: &str,
        params: Value,
        owner: Option<&str>,
    ) -> Result<Value, ApiError> {
        match method {
            "clipboard.history.revision" => self.history_revision().await,
            "clipboard.entry.details" => self.actions.details(decode(params)?).await,
            "clipboard.entry.thumbnail" => self.actions.thumbnail(decode(params)?).await,
            method if method.starts_with("clipboard.entry.") => {
                self.actions
                    .dispatch_entry(method, params, self.max_entry_bytes()?, owner)
                    .await
            }
            "clipboard.entries.delete" => self.actions.delete_entries(params).await,
            method if method.starts_with("clipboard.session.") => {
                self.actions.dispatch_session(method, params, owner).await
            }
            method if method.starts_with("clipboard.history.wipe.") => {
                self.dispatch_wipe(method, params, owner).await
            }
            _ => self.dispatch_policy(method, params).await,
        }
    }

    async fn history_revision(&self) -> Result<Value, ApiError> {
        let revision = self.actions.change_token().await?;
        Ok(json!({ "revision": revision, "snapshot_revision": revision.to_string() }))
    }

    async fn query_history(&self, params: Value, owner: Option<&str>) -> Result<Value, ApiError> {
        let collapse = self
            .settings
            .get()
            .map_err(settings_error)?
            .collapse_self_echoes;
        self.actions.query(decode(params)?, collapse, owner).await
    }

    async fn dispatch_wipe(
        &self,
        method: &str,
        params: Value,
        owner: Option<&str>,
    ) -> Result<Value, ApiError> {
        match method {
            "clipboard.history.wipe.prepare" => self.prepare_wipe(owner).await,
            "clipboard.history.wipe.commit" => self.commit_wipe(decode(params)?, owner).await,
            _ => Err(ApiError::unsupported("Unsupported wipe method")),
        }
    }

    async fn dispatch_policy(&self, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "clipboard.capture.setPaused" => self.set_paused(decode(params)?).await,
            "clipboard.capture.interactive" => {
                self.actions
                    .interactive_screenshot(decode(params)?, self.max_entry_bytes()?)
                    .await
            }
            "clipboard.capture.screenshot" => {
                self.actions
                    .capture_screenshot(decode(params)?, self.max_entry_bytes()?)
                    .await
            }
            "clipboard.settings.get" => self.get_settings().await,
            "clipboard.settings.update" => self.update_settings(decode(params)?).await,
            "clipboard.selection.publishSensitive" => {
                self.actions.publish_sensitive(decode(params)?).await
            }
            "clipboard.selection.publishText" => {
                self.actions
                    .publish_text(decode(params)?, self.max_entry_bytes()?)
                    .await
            }
            "clipboard.selection.publishFiles" => {
                self.actions
                    .publish_files(decode(params)?, self.max_entry_bytes()?)
                    .await
            }
            _ => Err(ApiError::new(
                "unsupported-method",
                format!("Unsupported clip-api method: {method}"),
            )),
        }
    }

    async fn prepare_wipe(&self, owner: Option<&str>) -> Result<Value, ApiError> {
        let id = format!("challenge-{}", Uuid::new_v4());
        let expires = Instant::now() + Duration::from_secs(30);
        let mut challenges = self.wipe_challenges.lock().await;
        challenges.retain(|_, challenge| challenge.expires > Instant::now());
        let view =
            json!({ "challenge": { "id": id, "confirmation": "WIPE", "expires_in_ms": 30000 } });
        challenges.insert(
            id,
            WipeChallenge {
                owner: owner.map(str::to_owned),
                expires,
            },
        );
        Ok(view)
    }

    fn max_entry_bytes(&self) -> Result<u64, ApiError> {
        self.settings
            .get()
            .map(|settings| settings.max_entry_bytes)
            .map_err(settings_error)
    }

    async fn get_settings(&self) -> Result<Value, ApiError> {
        let _ = self.settings.refresh_capture().await;
        let settings = self.settings.get().map_err(settings_error)?;
        let capture = self.settings.capture_state().map_err(settings_error)?;
        let retention = self
            .settings
            .retention_state()
            .await
            .map_err(settings_error)?;
        Ok(json!({ "settings": settings, "capture": capture, "retention": retention }))
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

    async fn commit_wipe(
        &self,
        params: WipeParams,
        owner: Option<&str>,
    ) -> Result<Value, ApiError> {
        if params.response != "WIPE" {
            return Err(ApiError::validation("wipe confirmation must be WIPE"));
        }
        let challenge = {
            let mut challenges = self.wipe_challenges.lock().await;
            if challenges
                .get(&params.challenge_id)
                .is_none_or(|challenge| challenge.owner.as_deref() != owner)
            {
                return Err(ApiError::new(
                    "stale-action",
                    "wipe challenge is unknown or already used",
                ));
            }
            challenges
                .remove(&params.challenge_id)
                .expect("checked under the same lock")
        };
        if challenge.expires <= Instant::now() {
            return Err(ApiError::new("stale-action", "wipe challenge expired"));
        }
        let capture = self.settings.quiesce().await.map_err(settings_error)?;
        self.actions.clear().await;
        let mut data = self.actions.wipe().await?;
        if let Err(error) = capture.resume().await {
            // Wipe has committed. Report failed capture recovery separately.
            data["operation"]["warning"] = json!(format!(
                "History was wiped; capture remains stopped: {error}"
            ));
        }
        Ok(data)
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

fn started_operation_id(data: &Value) -> Option<&str> {
    (data.pointer("/operation/status").and_then(Value::as_str) == Some("started"))
        .then(|| data.pointer("/operation/id").and_then(Value::as_str))
        .flatten()
}

fn operation_event(data: &Value, owner: Option<&str>) -> Option<LifecycleEvent> {
    let operation = data.get("operation")?;
    let status = operation["status"].as_str().unwrap_or("completed");
    (status != "started").then(|| LifecycleEvent {
        stream: protocol::stream::OPERATION,
        event: status.into(),
        data: json!({ "operation": operation }),
        audience: EventAudience::Owner(owner.map(str::to_owned)),
    })
}

fn policy_event(method: &str, data: &Value, owner: Option<&str>) -> Option<LifecycleEvent> {
    let (stream, event) = match method {
        "clipboard.capture.setPaused" | "clipboard.capture.screenshot" => {
            (protocol::stream::CAPTURE, "changed")
        }
        "clipboard.session.begin" => (
            protocol::stream::SESSION,
            if data["session"]["target_available"].as_bool() == Some(true) {
                "completed"
            } else {
                "fallback"
            },
        ),
        "clipboard.session.hidden" | "clipboard.session.end" => {
            (protocol::stream::SESSION, "completed")
        }
        "clipboard.entry.action"
            if matches!(
                data["operation"]["action"].as_str(),
                Some("paste" | "image-as-file")
            ) =>
        {
            (
                protocol::stream::SESSION,
                data["operation"]["status"].as_str().unwrap_or("completed"),
            )
        }
        _ => return None,
    };
    Some(LifecycleEvent {
        stream,
        event: event.into(),
        data: data.clone(),
        audience: if method == "clipboard.capture.setPaused" {
            EventAudience::Everyone
        } else {
            EventAudience::Owner(owner.map(str::to_owned))
        },
    })
}

fn settings_error(message: String) -> ApiError {
    ApiError::new("settings-error", message)
}

fn finish_request(method: &str, result: Result<Value, ApiError>) -> Value {
    match result {
        Ok(data) => success(data),
        Err(error) => {
            tracing::warn!(%method, code = %error.code, "clip-api request failed");
            error_response(error)
        }
    }
}

pub fn success(data: Value) -> Value {
    shelllist_daemon_core::success(API, data)
}

fn error_response(error: ApiError) -> Value {
    error_envelope(
        API,
        EnvelopeError::new(error.code, error.message).with_retryable(error.retryable),
    )
}

pub fn error(code: &str, message: String) -> Value {
    error_response(ApiError::new(code, message))
}

#[cfg(test)]
mod tests {
    use super::ApiService;
    use crate::{backend::ClipboardBackend, ringboard::RingboardBackend};
    use std::{sync::Arc, time::Duration};
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn wipe_challenges_are_owner_scoped_single_use_and_released() {
        use serde_json::json;
        let api = ApiService::new(Arc::new(crate::fake::FakeBackend::default()));
        let owner = Some(":1.1".to_owned());
        let prepared = api
            .dispatch_owned("clipboard.history.wipe.prepare", json!({}), owner.clone())
            .await;
        let id = prepared["data"]["challenge"]["id"].as_str().unwrap();
        let params = || json!({"challenge_id":id, "response":"WIPE"});
        for foreign in [None, Some(":1.2".to_owned())] {
            let result = api
                .dispatch_owned("clipboard.history.wipe.commit", params(), foreign)
                .await;
            assert_eq!(result["error"]["code"], "stale-action");
            assert!(api.wipe_challenges.lock().await.contains_key(id));
        }
        let result = api
            .dispatch_owned("clipboard.history.wipe.commit", params(), owner.clone())
            .await;
        // Unavailable capture can acknowledge pause, but cannot resume. Only
        // the empty fake history is wiped; no host capture or storage is used.
        assert_eq!(result["ok"], true);
        assert!(result["data"]["operation"]["warning"].is_string());
        assert!(!api.wipe_challenges.lock().await.contains_key(id));
        assert_eq!(
            api.dispatch_owned("clipboard.history.wipe.commit", params(), owner.clone())
                .await["error"]["code"],
            "stale-action"
        );
        for other in [owner.clone(), Some(":1.2".into()), None] {
            api.dispatch_owned("clipboard.history.wipe.prepare", json!({}), other)
                .await;
        }
        api.release_owner(":1.1").await;
        assert_eq!(api.wipe_challenges.lock().await.len(), 2);
        assert_eq!(api.resource_owners().await, [":1.2".to_owned()].into());
    }

    #[test]
    fn lifecycle_receipts_are_private_but_capture_configuration_remains_shared() {
        use serde_json::json;
        for owner in [None, Some(":1.1")] {
            for method in [
                "clipboard.session.begin",
                "clipboard.session.hidden",
                "clipboard.session.end",
                "clipboard.capture.screenshot",
                "clipboard.entry.action",
            ] {
                let data = json!({"session":{"id":"private-session", "target_available":true},
                    "operation":{"id":"private-operation", "action":"paste", "status":"completed"}});
                for event in [
                    super::policy_event(method, &data, owner),
                    super::operation_event(&data, owner),
                ] {
                    let event = event.unwrap();
                    assert!(event.visible_to(owner));
                    assert!(!event.visible_to(Some(":1.2")));
                    assert_eq!(event.visible_to(None), owner.is_none());
                }
            }
        }
        let configuration = super::policy_event(
            "clipboard.capture.setPaused",
            &json!({"capture":{"paused":true}}),
            Some(":1.1"),
        )
        .unwrap();
        assert!(configuration.visible_to(Some(":1.2")));
        assert!(configuration.visible_to(None));
    }

    #[tokio::test]
    async fn asynchronous_receipts_and_release_are_owned_even_after_early_completion() {
        let backend = Arc::new(RingboardBackend::default());
        let api = Arc::new(ApiService::new(backend.clone()));
        let mut receipts = api.lifecycle_events();
        let raw = api.operation_events();
        let watched = api.clone();
        let observer = tokio::spawn(async move { watched.observe_operations(raw).await });
        let (finish_a, wait_a) = oneshot::channel();
        let (_finish_b, wait_b) = oneshot::channel();
        let a = backend.test_operation(wait_a).unwrap();
        let b = backend.test_operation(wait_b).unwrap();
        finish_a.send(true).unwrap();
        // Force terminal-before-registration, without relying on broadcast delivery.
        tokio::time::timeout(Duration::from_secs(2), async {
            while backend.active_operations().unwrap().contains(&a.id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        api.track_operation(a.clone(), Some(":1.a".into()));
        api.track_operation(b.clone(), Some(":1.b".into()));
        let first = receipts.recv().await.unwrap();
        assert_eq!(first.event, "completed");
        assert!(first.visible_to(Some(":1.a")));
        assert!(!first.visible_to(Some(":1.b")));
        assert!(!first.visible_to(None));
        let second = receipts.recv().await.unwrap();
        assert!(second.visible_to(Some(":1.b")));
        api.release_owner(":1.a").await;
        assert!(backend.active_operations().unwrap().contains(&b.id));
        api.release_owner(":1.b").await;
        assert!(!backend.active_operations().unwrap().contains(&b.id));
        let cancelled = receipts.recv().await.unwrap();
        assert_eq!(cancelled.event, "cancelled");
        assert!(!cancelled.visible_to(Some(":1.a")));
        // Loss can precede registration. The transport's post-dispatch cleanup
        // must also cancel that newly accepted work, without a permanent tombstone.
        let (_finish, waiting) = oneshot::channel();
        let late = backend.test_operation(waiting).unwrap();
        api.track_operation(late.clone(), Some(":1.a".into()));
        api.release_owner(":1.a").await;
        assert!(!backend.active_operations().unwrap().contains(&late.id));
        observer.abort();
        let _ = observer.await;
    }

    #[tokio::test]
    async fn ownership_tracks_live_jobs_without_relying_on_event_delivery() {
        let backend = Arc::new(RingboardBackend::default());
        let api = ApiService::new(backend.clone());
        for early in [false, true] {
            for success in [false, true] {
                let mut events = backend.operation_events();
                let (finish, finished) = oneshot::channel();
                let operation = backend.test_operation(finished).unwrap();
                assert_eq!(events.recv().await.unwrap().status, "started");
                if !early {
                    api.track_operation(operation.clone(), Some(":1.1".into()));
                    assert!(api.operations.lock().unwrap().records[&operation.id].active());
                }
                finish.send(success).unwrap();
                let terminal = tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    terminal.status,
                    if success { "completed" } else { "failed" }
                );
                // No API event observer: the snapshot also covers lost/lagged events.
                if early {
                    api.track_operation(operation.clone(), Some(":1.1".into()));
                }
                api.refresh_operations();
                let book = api.operations.lock().unwrap();
                let record = &book.records[&operation.id];
                assert_eq!(record.operation, terminal);
                assert!(!record.active());
            }
        }
        for owner in [None, Some(":1.1")] {
            let (_finish, finished) = oneshot::channel();
            let operation = backend.test_operation(finished).unwrap();
            // Missing records are never equivalent to explicitly unowned jobs.
            assert!(!api.cancel_operation(&operation.id).await);
            assert!(backend.active_operations().unwrap().contains(&operation.id));
            api.track_operation(operation.clone(), owner.map(str::to_owned));
            assert!(
                !api.cancel_operation_owned(&operation.id, Some(":1.2"))
                    .await
            );
            assert!(api.cancel_operation_owned(&operation.id, owner).await);
            assert!(!api.cancel_operation_owned(&operation.id, owner).await);
            assert_eq!(
                api.operations.lock().unwrap().records[&operation.id]
                    .operation
                    .status,
                "cancelled"
            );
        }
    }
}
