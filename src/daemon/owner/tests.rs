//! Real private D-Bus, fake clipboard, no capture initialization or host helpers.
use super::*;
use crate::{
    daemon::{BUS_NAME, ClipDaemon, INTERFACE, OBJECT_PATH},
    fake::{DetailsGate, FakeBackend},
    model::{EntryDetails, EntryKind, EntrySummary},
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(5);
struct Bus {
    child: Child,
    address: String,
    _directory: tempfile::TempDir,
}
impl Bus {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus");
        let child = Command::new("dbus-daemon")
            .args([
                "--config-file",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/test_support/owner-session.conf"
                ),
                "--nofork",
                "--nopidfile",
                "--print-address=1",
            ])
            .arg(format!("--address=unix:path={}", path.display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut bus = Self {
            child,
            address: String::new(),
            _directory: directory,
        };
        assert!(
            BufReader::new(bus.child.stdout.take().unwrap())
                .read_line(&mut bus.address)
                .unwrap()
                > 0,
            "private bus must start"
        );
        bus.address = bus.address.trim().into();
        bus
    }
    async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .await
            .unwrap()
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn entry() -> EntryDetails {
    EntryDetails {
        entry: EntrySummary {
            id: "fixture".into(),
            revision: 1,
            kind: EntryKind::Text,
            mime: "text/plain".into(),
            byte_size: 4,
            favorite: false,
            current: false,
            preview: "test".into(),
        },
        text: Some("test".into()),
        files: vec![],
        image: None,
        preview_truncated: false,
    }
}
async fn proxy(connection: &zbus::Connection) -> zbus::Proxy<'_> {
    zbus::Proxy::new(connection, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .unwrap()
}
async fn call(proxy: &zbus::Proxy<'_>, method: &str, params: Value) -> Value {
    let response: String = timeout(DEADLINE, proxy.call("Call", &(method, params.to_string())))
        .await
        .unwrap()
        .unwrap();
    serde_json::from_str(&response).unwrap()
}
async fn event(stream: &mut zbus::proxy::SignalStream<'_>) -> Value {
    let message = timeout(DEADLINE, stream.next()).await.unwrap().unwrap();
    let (_, json): (String, String) = message.body().deserialize().unwrap();
    serde_json::from_str(&json).unwrap()
}
async fn released(api: &ApiService, owner: &str) {
    timeout(DEADLINE, async {
        while api.resource_owners().await.contains(owner) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn private_bus_scopes_leases_receipts_and_disconnect_cleanup() {
    let bus = Bus::new();
    let server = bus.connect().await;
    server.request_name(BUS_NAME).await.unwrap();
    let api = Arc::new(ApiService::new(Arc::new(FakeBackend::with_entries(vec![
        entry(),
    ]))));
    let tasks = TaskGroup::default();
    let owners = Owners::start(server.clone(), api.clone(), &tasks)
        .await
        .unwrap();
    let daemon = ClipDaemon::new(api.clone(), tokio::sync::broadcast::channel(32).0, owners);
    let subscriptions = daemon.subscriptions.clone();
    server
        .object_server()
        .at(OBJECT_PATH, daemon)
        .await
        .unwrap();
    let a = bus.connect().await;
    let b = bus.connect().await;
    let owner_a = a.unique_name().unwrap().to_string();
    let owner_b = b.unique_name().unwrap().to_string();
    let pa = proxy(&a).await;
    let pb = proxy(&b).await;
    let mut events_a = pa.receive_signal("Event").await.unwrap();
    let mut events_b = pb.receive_signal("Event").await.unwrap();
    for proxy in [&pa, &pb] {
        let response: String = proxy
            .call("Subscribe", &(vec!["clipboard.session"],))
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&response).unwrap()["ok"],
            true
        );
    }
    assert_eq!(event(&mut events_a).await["event"], "subscribed");
    assert_eq!(event(&mut events_b).await["event"], "subscribed");
    let edit_a = call(
        &pa,
        "clipboard.entry.edit.begin",
        json!({"entry_id":"fixture", "revision":1}),
    )
    .await;
    let edit_b = call(
        &pb,
        "clipboard.entry.edit.begin",
        json!({"entry_id":"fixture", "revision":1}),
    )
    .await;
    assert_eq!(edit_a["ok"], true);
    assert_eq!(edit_b["ok"], true);
    let foreign = call(
        &pb,
        "clipboard.entry.edit.cancel",
        json!({"edit_id":edit_a["data"]["edit"]["id"]}),
    )
    .await;
    assert_eq!(foreign["data"]["edit"]["cancelled"], false);
    // Ending an unknown session is safe/idempotent and does not invoke hyprctl.
    call(&pa, "clipboard.session.end", json!({"session_id":"a-only"})).await;
    call(&pb, "clipboard.session.end", json!({"session_id":"b-only"})).await;
    assert_eq!(
        event(&mut events_a).await["data"]["session"]["id"],
        "a-only"
    );
    assert_eq!(
        event(&mut events_b).await["data"]["session"]["id"],
        "b-only"
    );
    assert!(
        timeout(Duration::from_millis(40), events_a.next())
            .await
            .is_err()
    );
    assert!(
        timeout(Duration::from_millis(40), events_b.next())
            .await
            .is_err()
    );
    drop(events_a);
    drop(pa);
    a.close().await.unwrap();
    released(&api, &owner_a).await;
    assert!(api.resource_owners().await.contains(&owner_b));
    // A reconnect cannot inherit the old lease, even with its exact opaque ID.
    let c = bus.connect().await;
    let pc = proxy(&c).await;
    let stale = call(
        &pc,
        "clipboard.entry.edit.commit",
        json!({"edit_id":edit_a["data"]["edit"]["id"], "value":"stale"}),
    )
    .await;
    assert_eq!(stale["error"]["code"], "edit-error");
    let peer = call(
        &pb,
        "clipboard.entry.edit.commit",
        json!({"edit_id":edit_b["data"]["edit"]["id"], "value":"peer"}),
    )
    .await;
    assert_eq!(peer["data"]["entry"]["text"], "peer");
    subscriptions.shutdown().await;
    tasks.shutdown().await;
}

#[tokio::test]
async fn owner_loss_before_registration_cleans_up_the_late_accepted_lease() {
    for loss in ["owner", "monitor", "shutdown"] {
        late_lease(loss).await;
    }
}

async fn late_lease(loss: &str) {
    let bus = Bus::new();
    let server = bus.connect().await;
    let a = bus.connect().await;
    let owner = a.unique_name().unwrap().to_string();
    let gate = Arc::new(DetailsGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut backend = FakeBackend::with_entries(vec![entry()]);
    backend.details_gate = Some(gate.clone());
    let api = Arc::new(ApiService::new(Arc::new(backend)));
    let tasks = TaskGroup::default();
    let owners = Owners::start(server.clone(), api.clone(), &tasks)
        .await
        .unwrap();
    let response = owners
        .dispatch(
            &api,
            "clipboard.history.wipe.prepare",
            json!({}),
            owner.clone(),
        )
        .await;
    assert_eq!(response["ok"], true);
    let worker_api = api.clone();
    let worker_owner = owner.clone();
    let worker_owners = owners.clone();
    let worker = tokio::spawn(async move {
        worker_owners
            .dispatch(
                &worker_api,
                "clipboard.entry.edit.begin",
                json!({"entry_id":"fixture", "revision":1}),
                worker_owner,
            )
            .await
    });
    timeout(DEADLINE, gate.entered.notified()).await.unwrap();
    match loss {
        "monitor" => server.close().await.unwrap(),
        "shutdown" => owners.shutdown(&api).await,
        _ => a.close().await.unwrap(),
    }
    // The earlier challenge proves the loss was handled before the new lease
    // exists. Merely waiting for NameHasOwner=false would not prove this race.
    released(&api, &owner).await;
    gate.release.add_permits(1);
    let response = timeout(DEADLINE, worker).await.unwrap().unwrap();
    assert_eq!(response["ok"], true);
    assert!(!api.resource_owners().await.contains(&owner));
    let cancelled = api
        .dispatch_owned(
            "clipboard.entry.edit.cancel",
            json!({"edit_id":response["data"]["edit"]["id"]}),
            Some(owner.clone()),
        )
        .await;
    assert_eq!(cancelled["data"]["edit"]["cancelled"], false);
    let rejected = owners
        .dispatch(&api, "clipboard.history.wipe.prepare", json!({}), owner)
        .await;
    assert_eq!(rejected["error"]["code"], "owner-unavailable");
    tasks.shutdown().await;
}
