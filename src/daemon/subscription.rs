use std::{sync::Arc, time::Duration};

use futures::StreamExt;
use serde_json::json;
use tokio::time::MissedTickBehavior;
use zbus::{names::UniqueName, object_server::SignalEmitter};

use crate::{api, protocol};

use super::{ClipDaemon, emit_event};

#[derive(Clone, Copy)]
struct RequestedStreams {
    history: bool,
    current: bool,
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
        })
    }

    const fn watches_clipboard(self) -> bool {
        self.history || self.current
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
    let api_service = Arc::clone(&daemon.api);
    let subscriptions = Arc::clone(&daemon.subscriptions);
    let event_revision = Arc::clone(&daemon.event_revision);
    let task_id = id.clone();
    let task_streams = streams.clone();
    let connection = destination.connection().clone();
    let (start_task, task_ready) = tokio::sync::oneshot::channel();

    let task = tokio::spawn(async move {
        if task_ready.await.is_err() {
            return;
        }
        for stream in &task_streams {
            emit_event(&destination, stream, "subscribed", &task_id, None).await;
        }
        if requested.watches_clipboard() {
            tokio::select! {
                () = poll_history(destination, api_service, event_revision, task_id.clone(), requested) => {}
                () = wait_for_owner_loss(connection, owner) => {}
            }
        } else {
            wait_for_owner_loss(connection, owner).await;
        }
        subscriptions.lock().await.remove(&task_id);
        tracing::debug!(subscription_id = %task_id, "clipboard subscription ended");
    });
    daemon.subscriptions.lock().await.insert(id.clone(), task);
    if start_task.send(()).is_err() {
        if let Some(task) = daemon.subscriptions.lock().await.remove(&id) {
            task.abort();
        }
        return api::error(
            "subscription-unavailable",
            "Subscription task could not be started".into(),
        )
        .to_string();
    }
    tracing::debug!(subscription_id = %id, "clipboard subscription started");
    api::success(json!({ "subscription": { "id": id, "streams": streams } })).to_string()
}

async fn poll_history(
    emitter: SignalEmitter<'static>,
    api_service: Arc<crate::api::ApiService>,
    event_revision: Arc<std::sync::atomic::AtomicU64>,
    subscription_id: String,
    requested: RequestedStreams,
) {
    let mut previous = None;
    let mut unavailable = false;
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        match api_service.change_token().await {
            Ok(token) => {
                if history_changed(&mut previous, &mut unavailable, token) {
                    let revision =
                        event_revision.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    emit_requested(
                        &emitter,
                        &subscription_id,
                        requested,
                        ("reset", "changed"),
                        json!({ "data": { "revision": revision, "change": "reset" } }),
                    )
                    .await;
                }
            }
            Err(error) if !unavailable => {
                unavailable = true;
                emit_requested(
                    &emitter,
                    &subscription_id,
                    requested,
                    ("unavailable", "unavailable"),
                    error,
                )
                .await;
            }
            Err(_) => {}
        }
    }
}

fn history_changed(previous: &mut Option<u64>, unavailable: &mut bool, token: u64) -> bool {
    std::mem::replace(unavailable, false)
        || previous.replace(token).is_some_and(|value| value != token)
}

async fn emit_requested(
    emitter: &SignalEmitter<'_>,
    subscription_id: &str,
    requested: RequestedStreams,
    events: (&str, &str),
    data: serde_json::Value,
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
