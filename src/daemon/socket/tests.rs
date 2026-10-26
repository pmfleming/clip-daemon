use super::*;
use crate::{
    api::ApiService,
    backend::ClipboardBackend,
    daemon::{
        BUS_NAME, INTERFACE,
        owner::{
            Owners,
            tests::{Bus, entry},
        },
    },
    fake::{DetailsGate, FakeBackend},
};
use serde_json::json;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::oneshot,
    time::{Duration, timeout},
};

const DEADLINE: Duration = Duration::from_secs(5);
type Peer = BufReader<UnixStream>;
struct Fixture {
    _bus: Bus,
    directory: tempfile::TempDir,
    server: zbus::Connection,
    api: Arc<ApiService>,
    owners: Owners,
    tasks: shelllist_daemon_tokio::TaskGroup,
    subscriptions: Arc<shelllist_daemon_tokio::OwnedTaskRegistry>,
}
impl Fixture {
    async fn new(backend: Arc<dyn ClipboardBackend>) -> Self {
        let bus = Bus::new();
        let server = bus.connect().await;
        let api = Arc::new(ApiService::new(backend));
        let tasks = shelllist_daemon_tokio::TaskGroup::default();
        let owners = Owners::start(server.clone(), api.clone(), &tasks)
            .await
            .unwrap();
        let daemon = ClipDaemon::new(
            api.clone(),
            tokio::sync::broadcast::channel(32).0,
            owners.clone(),
        );
        let subscriptions = daemon.subscriptions.clone();
        server
            .object_server()
            .at(OBJECT_PATH, daemon)
            .await
            .unwrap();
        server.request_name(BUS_NAME).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _bus: bus,
            directory,
            server,
            api,
            owners,
            tasks,
            subscriptions,
        }
    }
    async fn listen(
        &self,
        config: JsonlSocketConfig,
    ) -> (
        oneshot::Sender<()>,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let listener = JsonlListener::bind(
            &self.directory.path().canonicalize().unwrap(),
            "clip-daemon",
        )
        .await
        .unwrap();
        let (stop, stopped) = oneshot::channel();
        let service = Arc::new(Service(self.server.clone()));
        let task =
            tokio::spawn(
                listener.serve(service, crate::client::ClipCorrelation, config, async {
                    let _ = stopped.await;
                }),
            );
        (stop, task)
    }
    async fn peer(&self) -> Peer {
        BufReader::new(
            UnixStream::connect(self.directory.path().join("clip-daemon.sock"))
                .await
                .unwrap(),
        )
    }
    async fn shutdown(
        self,
        stop: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let _ = stop.send(());
        timeout(DEADLINE, task).await.unwrap().unwrap().unwrap();
        self.subscriptions.shutdown().await;
        self.owners.shutdown(&self.api).await;
        self.server.close().await.unwrap();
        self.tasks.shutdown().await;
    }
}
async fn request(peer: &mut Peer, request: Value, events: &mut Vec<Value>) -> Value {
    peer.get_mut()
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    loop {
        let mut line = String::new();
        assert!(
            timeout(DEADLINE, peer.read_line(&mut line))
                .await
                .unwrap()
                .unwrap()
                > 0
        );
        let record: Value = serde_json::from_str(&line).unwrap();
        if record["kind"] == "response" && record["id"] == request["id"] {
            assert_eq!(record["route"], request["route"]);
            return record["response"].clone();
        }
        assert_eq!(record["kind"], "event");
        assert!(events.len() < 128);
        events.push(record);
    }
}
fn call(id: &str, method: &str, params: Value) -> Value {
    json!({"op":"call", "id":id, "method":method, "params":params,
        "route":{"consumerId":"fixture", "localId":id, "generation":3, "kind":"call"}})
}
async fn no_active(backend: &dyn ClipboardBackend, id: &str) {
    timeout(DEADLINE, async {
        while backend.active_operations().unwrap().contains(id) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn direct_ingress_shares_validation_scopes_operations_and_preserves_peers() {
    let (_finish_a, waiting_a) = oneshot::channel();
    let (finish_b, waiting_b) = oneshot::channel();
    let backend = Arc::new(
        FakeBackend::with_entries(vec![entry()]).with_operations(vec![waiting_a, waiting_b]),
    );
    let fixture = Fixture::new(backend.clone()).await;
    let (stop, task) = fixture.listen(config()).await;
    let dbus = fixture._bus.connect().await;
    let proxy = zbus::Proxy::new(&dbus, BUS_NAME, OBJECT_PATH, INTERFACE)
        .await
        .unwrap();
    let mut a = fixture.peer().await;
    let mut b = fixture.peer().await;
    let mut events = Vec::new();
    for (method, params) in [
        ("clipboard.history.query", json!({"fuzzy":true})),
        ("clipboard.history.revision", json!({})),
        ("clipboard.history.query", json!({"limit":201})),
        (
            "clipboard.operation.status",
            json!({"operation_id":"missing"}),
        ),
        ("unknown", json!({})),
    ] {
        let bus: String = proxy
            .call("Call", &(method, params.to_string()))
            .await
            .unwrap();
        let socket = request(&mut a, call("parity", method, params), &mut events).await;
        assert_eq!(socket, serde_json::from_str::<Value>(&bus).unwrap());
    }
    let sub = request(
        &mut a,
        json!({"id":"sub-a", "op":"subscribe", "streams":crate::protocol::STREAMS}),
        &mut events,
    )
    .await;
    let peer_sub = request(
        &mut b,
        json!({"id":"sub-b", "op":"subscribe", "streams":crate::protocol::STREAMS}),
        &mut events,
    )
    .await;
    assert_eq!(sub["ok"], true);
    assert_eq!(peer_sub["ok"], true);
    let cancel_foreign = request(
        &mut b,
        json!({"id":"cancel", "op":"cancel", "request_id":sub["data"]["subscription"]["id"]}),
        &mut events,
    )
    .await;
    assert_eq!(cancel_foreign["error"]["code"], "request-not-found");
    let op_a = request(
        &mut a,
        call(
            "a",
            "clipboard.capture.interactive",
            json!({"mode":"region"}),
        ),
        &mut events,
    )
    .await;
    let op_b = request(
        &mut b,
        call(
            "b",
            "clipboard.capture.interactive",
            json!({"mode":"region"}),
        ),
        &mut events,
    )
    .await;
    let a_id = op_a["data"]["operation"]["id"].as_str().unwrap();
    let b_id = op_b["data"]["operation"]["id"].as_str().unwrap();
    assert_ne!(a_id, b_id);
    assert_eq!(
        request(
            &mut b,
            call(
                "foreign",
                "clipboard.operation.status",
                json!({"operation_id":a_id})
            ),
            &mut events
        )
        .await["error"]["code"],
        "operation-unknown"
    );
    assert_eq!(
        request(
            &mut b,
            json!({"id":"foreign-cancel", "op":"cancel", "request_id":a_id}),
            &mut events
        )
        .await["error"]["code"],
        "request-not-found"
    );
    fixture.owners.shutdown(&fixture.api).await; // unrelated D-Bus loss is not socket ownership loss
    assert!(backend.active_operations().unwrap().contains(a_id));
    drop(a);
    no_active(backend.as_ref(), a_id).await;
    assert!(backend.active_operations().unwrap().contains(b_id));
    finish_b.send(true).unwrap();
    no_active(backend.as_ref(), b_id).await;
    events.clear();
    let terminal = request(
        &mut b,
        call(
            "status",
            "clipboard.operation.status",
            json!({"operation_id":b_id}),
        ),
        &mut events,
    )
    .await;
    assert_eq!(terminal["data"]["operation"]["status"], "completed");
    assert!(
        events
            .iter()
            .all(|event| event["event"]["data"]["operation"]["id"] != a_id)
    );
    let mut fresh = fixture.peer().await;
    assert_eq!(
        request(
            &mut fresh,
            call(
                "old",
                "clipboard.operation.status",
                json!({"operation_id":b_id})
            ),
            &mut events
        )
        .await["error"]["code"],
        "operation-unknown"
    );
    assert_eq!(request(&mut b, json!({"id":"cancel-sub", "op":"cancel", "request_id":peer_sub["data"]["subscription"]["id"]}), &mut events).await["ok"], true);
    drop(fresh);
    drop(b);
    drop(proxy);
    drop(dbus);
    fixture.shutdown(stop, task).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disconnect_before_acknowledgement_releases_late_accepted_work_without_replay() {
    let (_finish, waiting) = oneshot::channel();
    let gate = Arc::new(DetailsGate {
        entered: Default::default(),
        release: tokio::sync::Semaphore::new(0),
    });
    let mut backend = FakeBackend::default().with_operations(vec![waiting]);
    backend.operation_gate = Some(gate.clone());
    let backend = Arc::new(backend);
    let mut events = backend.operation_events();
    let fixture = Fixture::new(backend.clone()).await;
    let (stop, task) = fixture.listen(config()).await;
    let mut peer = fixture.peer().await;
    peer.get_mut()
        .write_all(
            format!(
                "{}\n",
                call(
                    "late",
                    "clipboard.capture.interactive",
                    json!({"mode":"region"})
                )
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    timeout(DEADLINE, gate.entered.notified()).await.unwrap();
    drop(peer);
    gate.release.add_permits(1);
    let started = timeout(DEADLINE, events.recv()).await.unwrap().unwrap();
    assert_eq!(started.status, "started");
    let terminal = timeout(DEADLINE, events.recv()).await.unwrap().unwrap();
    assert_eq!(terminal.status, "cancelled");
    assert_eq!(terminal.id, started.id);
    assert!(backend.active_operations().unwrap().is_empty());
    assert!(events.try_recv().is_err());
    fixture.shutdown(stop, task).await;
}
