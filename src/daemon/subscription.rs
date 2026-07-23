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
            || streams.iter().any(|requested| {
                !protocol::STREAMS
                    .iter()
                    .any(|(supported, _)| requested == supported)
            })
        {
            return None;
        }
        let wants = |target| streams.iter().any(|stream| stream == target);
        Some(Self {
            history: wants(protocol::stream::HISTORY),
            current: wants(protocol::stream::CURRENT),
        })
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
    let backend = Arc::clone(&daemon.backend);
    let subscriptions = Arc::clone(&daemon.subscriptions);
    let event_revision = Arc::clone(&daemon.event_revision);
    let task_id = id.clone();
    let task_streams = streams.clone();
    let connection = destination.connection().clone();

    let task = tokio::spawn(async move {
        for stream in &task_streams {
            emit_event(&destination, stream, "subscribed", &task_id, None).await;
        }
        tokio::select! {
            () = poll_history(destination.clone(), backend, event_revision, task_id.clone(), requested) => {}
            () = wait_for_owner_loss(connection, owner) => {}
        }
        subscriptions.lock().await.remove(&task_id);
        tracing::debug!(subscription_id = %task_id, "clipboard subscription ended");
    });
    daemon.subscriptions.lock().await.insert(id.clone(), task);
    tracing::debug!(subscription_id = %id, "clipboard subscription started");
    api::success(json!({ "subscription": { "id": id, "streams": streams } })).to_string()
}

async fn poll_history(
    emitter: SignalEmitter<'static>,
    backend: Arc<dyn crate::backend::ClipboardBackend>,
    event_revision: Arc<std::sync::atomic::AtomicU64>,
    subscription_id: String,
    requested: RequestedStreams,
) {
    if !requested.history && !requested.current {
        std::future::pending::<()>().await;
    }
    let mut previous = backend.change_token().await.ok();
    let mut timer = tokio::time::interval(Duration::from_millis(500));
    timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        timer.tick().await;
        let token = match backend.change_token().await {
            Ok(token) => token,
            Err(error) => {
                emit_event(
                    &emitter,
                    protocol::stream::HISTORY,
                    "unavailable",
                    &subscription_id,
                    Some(json!({ "error": { "code": error.kind.code(), "message": error.to_string() } })),
                )
                .await;
                continue;
            }
        };
        if previous.replace(token) == Some(token) {
            continue;
        }
        let revision = event_revision.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let data = Some(json!({ "data": { "revision": revision, "change": "reset" } }));
        if requested.history {
            emit_event(
                &emitter,
                protocol::stream::HISTORY,
                "reset",
                &subscription_id,
                data.clone(),
            )
            .await;
        }
        if requested.current {
            emit_event(
                &emitter,
                protocol::stream::CURRENT,
                "changed",
                &subscription_id,
                data,
            )
            .await;
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
