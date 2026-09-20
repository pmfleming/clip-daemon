use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::{Value, json};
use shelllist_daemon_tokio::{BroadcastEvent, forward_broadcast};
use tokio::{sync::broadcast::error::RecvError, time::MissedTickBehavior};
use zbus::{names::UniqueName, object_server::SignalEmitter};

use crate::{api, api::ApiService, protocol};

use super::{ClipDaemon, emit_event};

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
    history_events: tokio::sync::broadcast::Sender<HistoryUpdate>,
    id: String,
    streams: Vec<String>,
    requested: RequestedStreams,
}

struct LifecycleSubscription {
    destination: SignalEmitter<'static>,
    id: String,
    streams: Vec<String>,
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
            lifecycle: wants(protocol::stream::OPERATION)
                || wants(protocol::stream::CAPTURE)
                || wants(protocol::stream::SESSION),
        })
    }

    const fn watches_clipboard(self) -> bool {
        self.history || self.current
    }
}

impl SubscriptionTask {
    async fn run(self) {
        // Register receivers before acknowledging the subscription. Each history
        // subscriber also gets its own fresh baseline, even if polling is active.
        let history = self.requested.watches_clipboard().then(|| {
            receive_history(
                self.destination.clone(),
                self.history_events.subscribe(),
                Arc::clone(&self.api_service),
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
            LifecycleSubscription {
                destination: self.destination.clone(),
                id: self.id.clone(),
                streams: self.streams.clone(),
            }
            .run(self.api_service.lifecycle_events())
        });
        for stream in &self.streams {
            emit_event(&self.destination, stream, "subscribed", &self.id, None).await;
        }
        tokio::select! {
            () = await_optional(history) => {}
            () = await_optional(operations) => {}
            () = await_optional(lifecycle) => {}
        }
        tracing::debug!(subscription_id = %self.id, "clipboard subscription ended");
    }
}

async fn await_optional<F>(task: Option<F>)
where
    F: std::future::Future<Output = ()>,
{
    match task {
        Some(task) => task.await,
        None => std::future::pending().await,
    }
}

impl HistoryState {
    fn update(&mut self, result: Result<u64, Value>) -> Option<HistoryUpdate> {
        match result {
            Ok(token) => self
                .changed(token)
                .then_some(HistoryUpdate::Changed(Value::from(token))),
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
    let id = daemon.subscriptions.next_id("subscription");
    let destination = emitter.set_destination(owner.clone().into()).to_owned();
    let connection = destination.connection().clone();
    let task = SubscriptionTask {
        destination,
        api_service: Arc::clone(&daemon.api),
        history_events: daemon.history_events.clone(),
        id: id.clone(),
        streams: streams.clone(),
        requested,
    };
    if let Err(error) = daemon.subscriptions.spawn_for_owner(
        id.clone(),
        Some(owner.to_string()),
        &connection,
        task.run(),
    ) {
        return api::error("subscription-unavailable", error.to_string()).to_string();
    }
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
        observe_history_tick(&api_service, &event_revision, &events, &mut state).await;
    }
}

async fn observe_history_tick(
    api_service: &ApiService,
    event_revision: &AtomicU64,
    events: &tokio::sync::broadcast::Sender<HistoryUpdate>,
    state: &mut HistoryState,
) {
    if events.receiver_count() == 0 {
        *state = HistoryState::default();
        return;
    }
    let Some(update) = state.update(api_service.change_token().await) else {
        return;
    };
    let update = match update {
        HistoryUpdate::Changed(history_revision) => {
            let revision = event_revision.fetch_add(1, Ordering::Relaxed) + 1;
            HistoryUpdate::Changed(json!({ "data": {
                "revision": revision,
                "history_revision": history_revision,
                "change": "reset"
            } }))
        }
        unavailable => unavailable,
    };
    let _ = events.send(update);
}

async fn receive_history(
    emitter: SignalEmitter<'static>,
    mut events: tokio::sync::broadcast::Receiver<HistoryUpdate>,
    api_service: Arc<ApiService>,
    subscription_id: String,
    requested: RequestedStreams,
) {
    let initial = match api_service.change_token().await {
        Ok(token) => HistoryUpdate::Changed(json!({ "data": {
            "change": "reset", "reason": "initial", "history_revision": token
        } })),
        Err(error) => HistoryUpdate::Unavailable(error),
    };
    emit_update(&emitter, &subscription_id, requested, initial).await;
    loop {
        match events.recv().await {
            Ok(update) => emit_update(&emitter, &subscription_id, requested, update).await,
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(%subscription_id, skipped, "clipboard history events lagged");
                emit_update(
                    &emitter,
                    &subscription_id,
                    requested,
                    HistoryUpdate::Changed(json!({
                        "data": { "change": "reset", "reason": "lagged", "skipped": skipped }
                    })),
                )
                .await;
            }
            Err(RecvError::Closed) => return,
        }
    }
}

async fn poll_operations(
    emitter: SignalEmitter<'static>,
    events: tokio::sync::broadcast::Receiver<crate::model::OperationResult>,
    subscription_id: String,
) {
    let emitter = &emitter;
    let subscription_id = subscription_id.as_str();
    forward_broadcast(events, move |update| async move {
        match update {
            BroadcastEvent::Item(operation) => {
                let event = operation.status.clone();
                emit_event(
                    emitter,
                    protocol::stream::OPERATION,
                    &event,
                    subscription_id,
                    Some(json!({ "data": { "operation": operation } })),
                )
                .await;
            }
            BroadcastEvent::Lagged(skipped) => {
                tracing::warn!(%subscription_id, skipped, "clipboard operation events lagged");
                emit_event(
                    emitter,
                    protocol::stream::OPERATION,
                    "failed",
                    subscription_id,
                    Some(lag_data(skipped)),
                )
                .await;
            }
        }
    })
    .await;
}

impl LifecycleSubscription {
    async fn run(self, events: tokio::sync::broadcast::Receiver<api::LifecycleEvent>) {
        let sink = &self;
        forward_broadcast(events, move |update| async move {
            match update {
                BroadcastEvent::Item(update) => sink.emit(update).await,
                BroadcastEvent::Lagged(skipped) => sink.emit_lag(skipped).await,
            }
        })
        .await;
    }

    async fn emit(&self, update: api::LifecycleEvent) {
        if !self.streams.iter().any(|stream| stream == update.stream) {
            return;
        }
        emit_event(
            &self.destination,
            update.stream,
            &update.event,
            &self.id,
            Some(json!({ "data": update.data })),
        )
        .await;
    }

    async fn emit_lag(&self, skipped: u64) {
        tracing::warn!(subscription_id = %self.id, skipped, "clipboard lifecycle events lagged");
        for (stream, event) in self.streams.iter().filter_map(|stream| lag_event(stream)) {
            emit_event(
                &self.destination,
                stream,
                event,
                &self.id,
                Some(lag_data(skipped)),
            )
            .await;
        }
    }
}

fn lag_event(stream: &str) -> Option<(&str, &'static str)> {
    let event = match stream {
        protocol::stream::OPERATION => "failed",
        protocol::stream::CAPTURE => "changed",
        protocol::stream::SESSION => "fallback",
        _ => return None,
    };
    Some((stream, event))
}

fn lag_data(skipped: u64) -> Value {
    json!({
        "data": { "resync_required": true, "reason": "lagged", "skipped": skipped }
    })
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
