use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::{sync::Mutex, task::JoinHandle, time::MissedTickBehavior};
use zbus::{names::UniqueName, object_server::SignalEmitter};

use crate::{api, api::ApiService, protocol};

use super::{ClipDaemon, emit_event};

type Subscriptions = Arc<Mutex<HashMap<String, JoinHandle<()>>>>;

const HISTORY_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct RequestedStreams {
    history: bool,
    current: bool,
    operation: bool,
    lifecycle: bool,
}

struct SubscriptionTask {
    destination: SignalEmitter<'static>,
    api_service: Arc<ApiService>,
    subscriptions: Subscriptions,
    history_events: tokio::sync::broadcast::Sender<HistoryUpdate>,
    id: String,
    streams: Vec<String>,
    owner: UniqueName<'static>,
    requested: RequestedStreams,
}

#[derive(Default)]
struct HistoryState {
    previous: Option<u64>,
    unavailable: bool,
}

#[derive(Clone)]
pub(super) enum HistoryUpdate {
    Changed(Value),
    Unavailable(Value),
}

impl RequestedStreams {
    fn parse(streams: &[String]) -> Option<Self> {
        if streams.is_empty()
            || streams
                .iter()
                .any(|requested| !protocol::STREAMS.contains(&requested.as_str()))
        {
            return None;
        }
        let wants = |target| streams.iter().any(|stream| stream == target);
        Some(Self {
            history: wants(protocol::stream::HISTORY),
            current: wants(protocol::stream::CURRENT),
            operation: wants(protocol::stream::OPERATION),
            lifecycle: wants(protocol::stream::CAPTURE) || wants(protocol::stream::SESSION),
        })
    }

    const fn watches_clipboard(self) -> bool {
        self.history || self.current
    }
}

impl SubscriptionTask {
    async fn run(self) {
        let connection = self.destination.connection().clone();
        for stream in &self.streams {
            emit_event(&self.destination, stream, "subscribed", &self.id, None).await;
        }
        let history = self.requested.watches_clipboard().then(|| {
            receive_history(
                self.destination.clone(),
                self.history_events.subscribe(),
                self.id.clone(),
                self.requested,
            )
        });
        let operations = self.requested.operation.then(|| {
            poll_operations(
                self.destination.clone(),
                self.api_service.operation_events(),
                self.id.clone(),
            )
        });
        let lifecycle = self.requested.lifecycle.then(|| {
            poll_lifecycle(
                self.destination.clone(),
                self.api_service.lifecycle_events(),
                self.id.clone(),
                self.streams.clone(),
            )
        });
        tokio::select! {
            () = async { if let Some(task) = history { task.await } } => {}
            () = async { if let Some(task) = operations { task.await } } => {}
            () = async { if let Some(task) = lifecycle { task.await } } => {}
            () = wait_for_owner_loss(connection, self.owner) => {}
        }
        self.subscriptions.lock().await.remove(&self.id);
        tracing::debug!(subscription_id = %self.id, "clipboard subscription ended");
    }
}

impl HistoryState {
    fn update(&mut self, result: Result<u64, Value>) -> Option<HistoryUpdate> {
        match result {
            Ok(token) => self
                .changed(token)
                .then_some(HistoryUpdate::Changed(Value::Null)),
            Err(error) if !std::mem::replace(&mut self.unavailable, true) => {
                Some(HistoryUpdate::Unavailable(error))
            }
            Err(_) => None,
        }
    }

    fn changed(&mut self, token: u64) -> bool {
        // An initial reset closes the race between a frontend's first query and
        // establishing this subscription's history baseline.
        let previous = self.previous.replace(token);
        std::mem::replace(&mut self.unavailable, false)
            || previous.is_none_or(|previous| previous != token)
    }
}

pub(super) async fn start(
    daemon: &ClipDaemon,
    streams: Vec<String>,
    owner: UniqueName<'static>,
    emitter: SignalEmitter<'_>,
) -> String {
    let Some(requested) = RequestedStreams::parse(&streams) else {
        return api::error(
            "unsupported-stream",
            "Subscription contains no supported clip-api streams".into(),
        )
        .to_string();
    };
    let id = daemon.next_id("subscription");
    let destination = emitter.set_destination(owner.clone().into()).to_owned();
    let mut active = daemon.subscriptions.lock().await;
    let task = tokio::spawn(
        SubscriptionTask {
            destination,
            api_service: Arc::clone(&daemon.api),
            subscriptions: Arc::clone(&daemon.subscriptions),
            history_events: daemon.history_events.clone(),
            id: id.clone(),
            streams: streams.clone(),
            owner,
            requested,
        }
        .run(),
    );
    active.insert(id.clone(), task);
    drop(active);
    tracing::debug!(subscription_id = %id, "clipboard subscription started");
    api::success(json!({ "subscription": { "id": id, "streams": streams } })).to_string()
}

pub(super) async fn observe_history(
    api_service: Arc<ApiService>,
    event_revision: Arc<AtomicU64>,
    events: tokio::sync::broadcast::Sender<HistoryUpdate>,
) {
    let mut state = HistoryState::default();
    let mut timer = tokio::time::interval(HISTORY_POLL_INTERVAL);
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        if events.receiver_count() == 0 {
            state = HistoryState::default();
            continue;
        }
        if let Some(mut update) = state.update(api_service.change_token().await) {
            if matches!(update, HistoryUpdate::Changed(_)) {
                let revision = event_revision.fetch_add(1, Ordering::Relaxed) + 1;
                update = HistoryUpdate::Changed(
                    json!({ "data": { "revision": revision, "change": "reset" } }),
                );
            }
            let _ = events.send(update);
        }
    }
}

async fn receive_history(
    emitter: SignalEmitter<'static>,
    mut events: tokio::sync::broadcast::Receiver<HistoryUpdate>,
    subscription_id: String,
    requested: RequestedStreams,
) {
    loop {
        match events.recv().await {
            Ok(update) => emit_update(&emitter, &subscription_id, requested, update).await,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(%subscription_id, skipped, "clipboard history events lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn poll_operations(
    emitter: SignalEmitter<'static>,
    mut events: tokio::sync::broadcast::Receiver<crate::model::OperationResult>,
    subscription_id: String,
) {
    loop {
        match events.recv().await {
            Ok(operation) => {
                let event = operation.status.clone();
                emit_event(
                    &emitter,
                    protocol::stream::OPERATION,
                    &event,
                    &subscription_id,
                    Some(json!({ "data": { "operation": operation } })),
                )
                .await;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(%subscription_id, skipped, "clipboard operation events lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn poll_lifecycle(
    emitter: SignalEmitter<'static>,
    mut events: tokio::sync::broadcast::Receiver<api::LifecycleEvent>,
    subscription_id: String,
    streams: Vec<String>,
) {
    loop {
        match events.recv().await {
            Ok(update) if streams.iter().any(|stream| stream == update.stream) => {
                emit_event(
                    &emitter,
                    update.stream,
                    &update.event,
                    &subscription_id,
                    Some(json!({ "data": update.data })),
                )
                .await;
            }
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(%subscription_id, skipped, "clipboard lifecycle events lagged");
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn emit_update(
    emitter: &SignalEmitter<'_>,
    subscription_id: &str,
    requested: RequestedStreams,
    update: HistoryUpdate,
) {
    let (events, data) = match update {
        HistoryUpdate::Changed(data) => (("reset", "changed"), data),
        HistoryUpdate::Unavailable(error) => (("unavailable", "unavailable"), error),
    };
    emit_requested(emitter, subscription_id, requested, events, data).await;
}

async fn emit_requested(
    emitter: &SignalEmitter<'_>,
    subscription_id: &str,
    requested: RequestedStreams,
    events: (&str, &str),
    data: Value,
) {
    for (enabled, stream, event) in [
        (requested.history, protocol::stream::HISTORY, events.0),
        (requested.current, protocol::stream::CURRENT, events.1),
    ] {
        if enabled {
            emit_event(emitter, stream, event, subscription_id, Some(data.clone())).await;
        }
    }
}

async fn wait_for_owner_loss(connection: zbus::Connection, owner: UniqueName<'static>) {
    let Ok(proxy) = zbus::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await
    else {
        return;
    };
    let Ok(mut changes) = proxy.receive_signal("NameOwnerChanged").await else {
        return;
    };
    while let Some(message) = changes.next().await {
        let Ok((name, old_owner, new_owner)) =
            message.body().deserialize::<(String, String, String)>()
        else {
            continue;
        };
        if name == owner.as_str() && !old_owner.is_empty() && new_owner.is_empty() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{HistoryState, HistoryUpdate, RequestedStreams};
    use crate::protocol;

    #[test]
    fn requested_streams_reject_empty_and_unknown_subscriptions() {
        assert!(RequestedStreams::parse(&[]).is_none());
        assert!(RequestedStreams::parse(&["unknown".into()]).is_none());
        let requested = RequestedStreams::parse(&[
            protocol::stream::HISTORY.into(),
            protocol::stream::CURRENT.into(),
        ])
        .expect("supported streams");
        assert!(requested.history && requested.current && requested.watches_clipboard());
    }

    #[test]
    fn history_state_emits_initial_changes_outages_and_recovery_once() {
        let mut state = HistoryState::default();
        assert!(matches!(
            state.update(Ok(1)),
            Some(HistoryUpdate::Changed(_))
        ));
        assert!(state.update(Ok(1)).is_none());
        assert!(matches!(
            state.update(Ok(2)),
            Some(HistoryUpdate::Changed(_))
        ));
        assert!(matches!(
            state.update(Err(json!({ "error": "offline" }))),
            Some(HistoryUpdate::Unavailable(_))
        ));
        assert!(state.update(Err(json!({ "error": "offline" }))).is_none());
        assert!(matches!(
            state.update(Ok(2)),
            Some(HistoryUpdate::Changed(_))
        ));
    }
}
