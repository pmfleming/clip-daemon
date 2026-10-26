//! Real SIGKILL/restart of a test-only exported daemon. No Ringboard, Wayland,
//! capture worker, real clipboard publication or live desktop helper is used.
//! Target lookup/input injection use a private, builtin-only hyprctl script.
use super::*;
use crate::{
    backend::*,
    model::{
        BackendStatus, EntryDetails, EntryThumbnail, HistoryPage, OperationResult,
        ReplacementResult,
    },
};
use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Mutex,
};

const CHILD_TEST: &str = "daemon::socket::tests::process::real_process_crash_restart_keeps_lost_receipts_unknown_and_never_replays";

struct PrivateBus {
    child: Child,
    address: String,
    _directory: tempfile::TempDir,
}
impl PrivateBus {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bus");
        let address = format!("unix:path={}", path.display());
        let log = File::create(directory.path().join("bus.log")).unwrap();
        let child = Command::new("dbus-daemon")
            .args([
                "--config-file",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/test_support/owner-session.conf"
                ),
                "--nofork",
                "--nopidfile",
            ])
            .arg(format!("--address={address}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut bus = Self {
            child,
            address,
            _directory: directory,
        };
        timeout(DEADLINE, async {
            loop {
                assert!(
                    bus.child.try_wait().unwrap().is_none(),
                    "private no-activation bus exited"
                );
                if UnixStream::connect(&path).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("private bus startup deadline");
        bus
    }
}
impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// This backend deliberately does NOT use FakeBackend::with_operations: that
// helper uses RingboardBackend internally. Only inert history/edit methods are
// delegated to FakeBackend; asynchronous work is entirely in-memory here.
struct Backend {
    fake: FakeBackend,
    records: Mutex<HashMap<String, OperationResult>>,
    events: tokio::sync::broadcast::Sender<OperationResult>,
    audit: PathBuf,
}
impl Backend {
    fn audit(&self, kind: &str, id: &str) {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit)
            .unwrap();
        file.write_all(format!("{}\n", json!({"kind":kind,"id":id})).as_bytes())
            .unwrap();
        file.sync_data().unwrap();
    }
    fn complete(&self, id: &str) {
        let mut records = self.records.lock().unwrap();
        let record = records.get_mut(id).unwrap();
        assert_eq!(record.status, "started");
        record.status = "completed".into();
        self.audit("terminal", id);
        let _ = self.events.send(record.clone());
    }
}
#[async_trait::async_trait]
impl ClipboardBackend for Backend {
    fn operation_events(&self) -> tokio::sync::broadcast::Receiver<OperationResult> {
        self.events.subscribe()
    }
    fn active_operations(&self) -> BackendResult<HashSet<String>> {
        Ok(self
            .records
            .lock()
            .unwrap()
            .values()
            .filter(|op| op.status == "started")
            .map(|op| op.id.clone())
            .collect())
    }
    fn operation_status(&self, id: &str) -> BackendResult<Option<OperationResult>> {
        Ok(self.records.lock().unwrap().get(id).cloned())
    }
    async fn interactive_screenshot(
        &self,
        _: InteractiveScreenshot,
        _: u64,
    ) -> BackendResult<OperationResult> {
        let operation = OperationResult::with_id(
            uuid::Uuid::new_v4().to_string(),
            "screenshot",
            "started",
            "Private fake committing work",
        );
        self.audit("capture", &operation.id);
        self.records
            .lock()
            .unwrap()
            .insert(operation.id.clone(), operation.clone());
        let _ = self.events.send(operation.clone());
        Ok(operation)
    }
    async fn cancel_operation(&self, id: &str) -> BackendResult<bool> {
        // Accepted committing work cannot be rolled back. Admission must remain
        // occupied after owner cleanup until an authoritative terminal snapshot.
        self.audit("cancel", id);
        Ok(false)
    }
    async fn status(&self) -> BackendStatus {
        self.fake.status().await
    }
    async fn change_token(&self) -> BackendResult<u64> {
        self.fake.change_token().await
    }
    async fn query(&self, query: HistoryQuery) -> BackendResult<HistoryPage> {
        self.fake.query(query).await
    }
    async fn revision(&self, id: &str) -> BackendResult<u64> {
        self.fake.revision(id).await
    }
    async fn details(&self, id: &str, max: usize) -> BackendResult<EntryDetails> {
        self.fake.details(id, max).await
    }
    async fn thumbnail(
        &self,
        id: &str,
        rev: Option<u64>,
        edge: u32,
    ) -> BackendResult<EntryThumbnail> {
        self.fake.thumbnail(id, rev, edge).await
    }
    async fn capture_screenshot(
        &self,
        _: ScreenshotRegion,
        _: u64,
    ) -> BackendResult<OperationResult> {
        self.audit("capture-sync", "fixture");
        Ok(OperationResult::completed(
            "screenshot",
            "Private fake capture",
        ))
    }
    async fn publish(
        &self,
        mime: &str,
        bytes: Vec<u8>,
        max: u64,
    ) -> BackendResult<OperationResult> {
        self.audit("publish", "fixture");
        self.fake.publish(mime, bytes, max).await
    }
    async fn publish_files(
        &self,
        selection: FileSelection,
        max: u64,
    ) -> BackendResult<OperationResult> {
        self.audit("publish-files", "fixture");
        self.fake.publish_files(selection, max).await
    }
    async fn mutate(
        &self,
        id: &str,
        rev: Option<u64>,
        mutation: BackendMutation,
    ) -> BackendResult<OperationResult> {
        self.audit("mutate", id);
        self.fake.mutate(id, rev, mutation).await
    }
    async fn remove_many(&self, targets: &[EntryTarget]) -> BackendResult<OperationResult> {
        self.fake.remove_many(targets).await
    }
    async fn replace(
        &self,
        id: &str,
        rev: u64,
        mime: &str,
        bytes: &[u8],
    ) -> BackendResult<ReplacementResult> {
        self.audit("replace", id);
        self.fake.replace(id, rev, mime, bytes).await
    }
}

struct GatedService {
    service: Service,
    backend: Arc<Backend>,
    directory: PathBuf,
    phase: String,
}
impl LocalJsonlService for GatedService {
    async fn call(&self, session: &SocketSession, method: &str, params: Value) -> Result<Value> {
        self.backend.audit("owner", session.owner());
        let response = self.service.call(session, method, params).await?;
        let hold = (self.phase == "pre-ack" && method == "clipboard.capture.interactive")
            || (["publication", "paste-prepared"].contains(&self.phase.as_str())
                && method == "clipboard.entry.action")
            || (self.phase == "paste-hidden"
                && method == "clipboard.session.hidden"
                && response["ok"] == true)
            || (self.phase == "terminal-receipt"
                && method == "clipboard.operation.status"
                && response["data"]["operation"]["status"] == "completed");
        if hold {
            publish_marker(&self.directory, "held", response.to_string().as_bytes());
            // Post-dispatch gate: real admission/ownership has already run, but
            // the shared listener has not received the response to encode/send.
            std::future::pending::<()>().await;
        }
        Ok(response)
    }
    async fn subscribe(&self, session: &SocketSession, streams: Vec<String>) -> Result<Value> {
        self.service.subscribe(session, streams).await
    }
    async fn cancel(&self, session: &SocketSession, id: &str) -> Result<Value> {
        self.service.cancel(session, id).await
    }
    async fn disconnected(&self, session: &SocketSession) {
        self.service.disconnected(session).await;
    }
}

async fn private_daemon_child() {
    assert_eq!(std::env::var("CLIP_CRASH_CHILD").as_deref(), Ok("1"));
    timeout(Duration::from_secs(40), async {
        let directory = PathBuf::from(std::env::var_os("CLIP_CRASH_DIRECTORY").unwrap());
        let server =
            zbus::connection::Builder::address(std::env::var("CLIP_CRASH_BUS").unwrap().as_str())
                .unwrap()
                .build()
                .await
                .unwrap();
        let backend = Arc::new(Backend {
            fake: FakeBackend::with_entries(vec![entry()]),
            records: Default::default(),
            events: tokio::sync::broadcast::channel(64).0,
            audit: directory.join("audit"),
        });
        let api = Arc::new(ApiService::with_settings(
            backend.clone(),
            crate::settings::SettingsManager::isolated(),
        ));
        let tasks = shelllist_daemon_tokio::TaskGroup::default();
        let owners = Owners::start(server.clone(), api.clone(), &tasks)
            .await
            .unwrap();
        let daemon = ClipDaemon::new(api, tokio::sync::broadcast::channel(32).0, owners);
        server
            .object_server()
            .at(OBJECT_PATH, daemon)
            .await
            .unwrap();
        server.request_name(BUS_NAME).await.unwrap();
        let listener = JsonlListener::bind(&directory, "clip-daemon")
            .await
            .unwrap();
        let worker_directory = directory.clone();
        let worker_backend = backend.clone();
        tasks.spawn("private-completion-control", async move {
            loop {
                if let Ok(ids) = std::fs::read_to_string(worker_directory.join("finish")) {
                    std::fs::remove_file(worker_directory.join("finish")).unwrap();
                    for id in ids.lines() {
                        worker_backend.complete(id);
                    }
                    publish_marker(&worker_directory, "finished", b"ready");
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        publish_marker(&directory, "ready", b"ready");
        listener
            .serve(
                Arc::new(GatedService {
                    service: Service(server),
                    backend,
                    directory,
                    phase: std::env::var("CLIP_CRASH_PHASE").unwrap(),
                }),
                crate::client::ClipCorrelation,
                config(),
                std::future::pending(),
            )
            .await
            .unwrap();
    })
    .await
    .expect("private daemon child exceeded lifetime");
}

struct Process {
    child: Child,
}
impl Process {
    fn spawn(directory: &Path, bus: &PrivateBus, phase: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        for marker in ["ready", "held", "finished"] {
            let _ = std::fs::remove_file(directory.join(marker));
        }
        let home = directory.join("home");
        std::fs::create_dir_all(&home).unwrap();
        // Child PATH contains ONLY this fake. It cannot find host hyprctl,
        // clipboard tools, capture helpers, applications or notification tools.
        let tools = directory.join("fake-bin");
        std::fs::create_dir_all(&tools).unwrap();
        let shell = std::env::split_paths(&std::env::var_os("PATH").unwrap())
            .map(|path| path.join("bash"))
            .find(|path| path.is_file())
            .unwrap();
        let script = format!(
            "#!{}\n{}",
            shell.canonicalize().unwrap().display(),
            r#"case "$1" in
-j) printf '%s\n' '{"address":"0x123","class":"private-fixture"}' ;;
dispatch)
    case "$2" in
        *send_shortcut*|sendshortcut) printf '%s\n' '{"kind":"paste","id":"fixture"}' >> "$XDG_RUNTIME_DIR/audit" ;;
        *focus*|focuswindow) printf '%s\n' '{"kind":"focus","id":"fixture"}' >> "$XDG_RUNTIME_DIR/audit" ;;
        *) exit 64 ;;
    esac
    printf '%s\n' ok ;;
*) exit 64 ;;
esac
"#
        );
        let helper = tools.join("hyprctl");
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let log = File::create(directory.join(format!("child-{phase}.log"))).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("XDG_CACHE_HOME", &home)
            .env("XDG_DATA_HOME", &home)
            .env("XDG_STATE_HOME", &home)
            .env("XDG_RUNTIME_DIR", directory)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                "unix:path=/nonexistent-clip-crash-session",
            )
            .env(
                "DBUS_SYSTEM_BUS_ADDRESS",
                "unix:path=/nonexistent-clip-crash-system",
            )
            .env("WAYLAND_DISPLAY", "nonexistent-clip-crash-wayland")
            .env("PATH", tools)
            .env("CLIP_CRASH_CHILD", "1")
            .env("CLIP_CRASH_DIRECTORY", directory)
            .env("CLIP_CRASH_BUS", &bus.address)
            .env("CLIP_CRASH_PHASE", phase)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        Self { child }
    }
    fn kill(&mut self) {
        self.child.kill().unwrap(); // Unix Child::kill is SIGKILL, not graceful stop.
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(self.child.wait().unwrap().signal(), Some(9));
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
// Field drop order is intentional: reap the daemon before its private bus and
// runtime directory. All crash cases share this resource/ownership boundary.
struct ProcessFixture {
    process: Process,
    bus: PrivateBus,
    _directory: tempfile::TempDir,
    runtime: PathBuf,
}
impl ProcessFixture {
    async fn new(phase: &str) -> Self {
        let bus = PrivateBus::new().await;
        let directory = tempfile::tempdir().unwrap();
        let runtime = directory.path().canonicalize().unwrap();
        let process = Process::spawn(&runtime, &bus, phase);
        marker(&runtime, "ready").await;
        Self {
            process,
            bus,
            _directory: directory,
            runtime,
        }
    }
    async fn restart(&mut self) {
        assert!(
            self.process.child.try_wait().unwrap().is_some(),
            "kill and observe the old daemon before restart"
        );
        self.process = Process::spawn(&self.runtime, &self.bus, "restarted");
        marker(&self.runtime, "ready").await;
    }
}

fn publish_marker(directory: &Path, name: &str, bytes: &[u8]) {
    let temporary = directory.join(format!("{name}.tmp"));
    std::fs::write(&temporary, bytes).unwrap();
    std::fs::rename(temporary, directory.join(name)).unwrap();
}
async fn marker(directory: &Path, name: &str) -> String {
    timeout(DEADLINE, async {
        loop {
            if let Ok(value) = std::fs::read_to_string(directory.join(name)) {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        for entry in std::fs::read_dir(directory).unwrap().flatten() {
            if entry.file_name().to_string_lossy().starts_with("child-") {
                let mut bytes = Vec::new();
                if let Ok(file) = File::open(entry.path()) {
                    let _ = file.take(4096).read_to_end(&mut bytes);
                }
                eprintln!(
                    "private child diagnostic: {}",
                    String::from_utf8_lossy(&bytes)
                );
            }
        }
        panic!("private child marker deadline: {name}")
    })
}
async fn peer(directory: &Path) -> Peer {
    BufReader::new(
        UnixStream::connect(directory.join("clip-daemon.sock"))
            .await
            .unwrap(),
    )
}
async fn send(peer: &mut Peer, value: Value) {
    peer.get_mut()
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}
async fn eof(peer: &mut Peer) {
    timeout(DEADLINE, async {
        loop {
            let mut line = String::new();
            if peer.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["kind"],
                "event",
                "lost receipt unexpectedly delivered"
            );
        }
    })
    .await
    .unwrap();
}
fn audit(directory: &Path) -> Vec<Value> {
    std::fs::read_to_string(directory.join("audit"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
async fn complete(directory: &Path, ids: &[String]) {
    let _ = std::fs::remove_file(directory.join("finished"));
    let temporary = directory.join("finish.tmp");
    std::fs::write(&temporary, ids.join("\n")).unwrap();
    std::fs::rename(temporary, directory.join("finish")).unwrap();
    marker(directory, "finished").await;
}
async fn capture(peer: &mut Peer) -> String {
    let response = request(
        peer,
        call(
            "capture",
            "clipboard.capture.interactive",
            json!({"mode":"region"}),
        ),
        &mut Vec::new(),
    )
    .await;
    assert_eq!(response["ok"], true, "{response}");
    response["data"]["operation"]["id"].as_str().unwrap().into()
}
async fn status(peer: &mut Peer, id: &str) -> Value {
    request(
        peer,
        call(
            "status",
            "clipboard.operation.status",
            json!({"operation_id":id}),
        ),
        &mut Vec::new(),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_process_crash_restart_keeps_lost_receipts_unknown_and_never_replays() {
    if std::env::var("CLIP_CRASH_CHILD").as_deref() == Ok("1") {
        private_daemon_child().await;
        return;
    }
    timeout(Duration::from_secs(60), async {
        for phase in [
            "pre-ack",
            "accepted",
            "terminal-receipt",
            "terminal-event",
            "publication",
        ] {
            crash_case(phase).await;
        }
        for phase in ["paste-prepared", "paste-hidden", "paste-delivered"] {
            paste_case(phase).await;
        }
    })
    .await
    .unwrap();
}
async fn crash_case(phase: &str) {
    let mut fixture = ProcessFixture::new(phase).await;
    let directory = fixture.runtime.clone();
    let mut a = peer(&directory).await;
    let mut b = peer(&directory).await;
    let mut events = Vec::new();
    // Keep a peer with its own lease and prove foreign-owner access cannot
    // consume the crashing owner's edit or operation.
    let lease = request(
        &mut a,
        call(
            "edit",
            "clipboard.entry.edit.begin",
            json!({"entry_id":"fixture","revision":1}),
        ),
        &mut events,
    )
    .await;
    let edit_id = lease["data"]["edit"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        request(
            &mut b,
            call(
                "foreign-edit",
                "clipboard.entry.edit.cancel",
                json!({"edit_id":edit_id})
            ),
            &mut events
        )
        .await["data"]["edit"]["cancelled"],
        false
    );
    let operation_id;
    if phase == "pre-ack" || phase == "publication" {
        let request = if phase == "publication" {
            call(
                "lost",
                "clipboard.entry.action",
                json!({"entry_id":"fixture","revision":1,"action":"copy"}),
            )
        } else {
            call(
                "lost",
                "clipboard.capture.interactive",
                json!({"mode":"region"}),
            )
        };
        send(&mut a, request).await;
        let held: Value = serde_json::from_str(&marker(&directory, "held").await).unwrap();
        assert_eq!(held["ok"], true);
        // Test oracle only. The requester has NOT received this opaque ID.
        operation_id = held["data"]["operation"]["id"].as_str().unwrap().to_owned();
    } else {
        if phase == "terminal-event" {
            assert_eq!(
                request(
                    &mut a,
                    json!({"id":"sub","op":"subscribe","streams":["clipboard.operation"]}),
                    &mut events
                )
                .await["ok"],
                true
            );
        }
        operation_id = capture(&mut a).await;
        assert_eq!(
            status(&mut b, &operation_id).await["error"]["code"],
            "operation-unknown"
        );
        assert_eq!(
            request(
                &mut b,
                json!({"id":"foreign-cancel","op":"cancel","request_id":operation_id}),
                &mut events
            )
            .await["error"]["code"],
            "request-not-found"
        );
        if phase.starts_with("terminal-") {
            complete(&directory, std::slice::from_ref(&operation_id)).await;
            if phase == "terminal-receipt" {
                send(
                    &mut a,
                    call(
                        "lost-terminal",
                        "clipboard.operation.status",
                        json!({"operation_id":operation_id}),
                    ),
                )
                .await;
                let held: Value = serde_json::from_str(&marker(&directory, "held").await).unwrap();
                assert_eq!(held["data"]["operation"]["status"], "completed");
            } else {
                timeout(DEADLINE, async {
                    loop {
                        let mut line = String::new();
                        assert!(a.read_line(&mut line).await.unwrap() > 0);
                        let value: Value = serde_json::from_str(&line).unwrap();
                        if value["event"]["data"]["operation"]["id"] == operation_id
                            && value["event"]["data"]["operation"]["status"] == "completed"
                        {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
        }
    }
    let old_history = request(
        &mut b,
        call("healthy", "clipboard.history.query", json!({})),
        &mut events,
    )
    .await;
    assert_eq!(old_history["ok"], true);
    let old_epoch = old_history["data"]["_history"]["epoch"].as_str().unwrap();
    let old_owners: HashSet<String> = audit(&directory)
        .iter()
        .filter(|row| row["kind"] == "owner")
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(old_owners.len(), 2);
    fixture.process.kill();
    eof(&mut a).await;
    eof(&mut b).await; // A daemon crash invalidates all peers, not only one consumer.
    let before = audit(&directory);
    let effects = |rows: &[Value]| {
        rows.iter()
            .filter(|row| {
                [
                    "capture",
                    "capture-sync",
                    "publish",
                    "publish-files",
                    "mutate",
                    "replace",
                ]
                .iter()
                .any(|kind| row["kind"] == *kind)
            })
            .count()
    };
    assert_eq!(effects(&before), 1);
    // Restart same private runtime with the stale socket left by SIGKILL.
    fixture.restart().await;
    let mut fresh = peer(&directory).await;
    let mut fresh_peer = peer(&directory).await;
    for connection in [&mut fresh, &mut fresh_peer] {
        assert_eq!(
            status(connection, &operation_id).await["error"]["code"],
            "operation-unknown"
        );
        assert_eq!(
            request(
                connection,
                json!({"id":"cancel-old","op":"cancel","request_id":operation_id}),
                &mut events
            )
            .await["error"]["code"],
            "request-not-found"
        );
        assert_eq!(
            request(
                connection,
                call(
                    "old-edit",
                    "clipboard.entry.edit.commit",
                    json!({"edit_id":edit_id,"value":"synthetic retry forbidden"})
                ),
                &mut events
            )
            .await["error"]["code"],
            "edit-error"
        );
        assert_eq!(
            request(
                connection,
                call(
                    "old-hidden",
                    "clipboard.session.hidden",
                    json!({"session_id":"retired-fixture-session"})
                ),
                &mut events
            )
            .await["ok"],
            false
        );
        let baseline = request(
            connection,
            call("baseline", "clipboard.history.query", json!({})),
            &mut events,
        )
        .await;
        assert_eq!(baseline["ok"], true);
        let new_epoch = baseline["data"]["_history"]["epoch"].as_str().unwrap();
        assert_ne!(
            new_epoch, old_epoch,
            "restart establishes a fresh producer epoch"
        );
    }
    let after = audit(&directory);
    assert_eq!(
        effects(&after),
        1,
        "recovery must be read-only, never replay effects"
    );
    assert!(
        after[before.len()..]
            .iter()
            .filter(|row| row["kind"] == "owner")
            .all(|row| !old_owners.contains(row["id"].as_str().unwrap()))
    );
    // New work requires an explicit new request; it is not resurrection of an old ID.
    let new_id = capture(&mut fresh_peer).await;
    assert_ne!(new_id, operation_id);
    drop(fresh);
    assert_eq!(
        status(&mut fresh_peer, &new_id).await["data"]["operation"]["status"],
        "started"
    );
    fixture.process.kill();
    println!("CLIP_PROCESS_CRASH_PASS phase={phase}");
}

async fn paste_case(phase: &str) {
    let mut fixture = ProcessFixture::new(phase).await;
    let directory = fixture.runtime.clone();
    let mut a = peer(&directory).await;
    let mut b = peer(&directory).await;
    let mut events = Vec::new();
    let began = request(
        &mut a,
        call("begin", "clipboard.session.begin", json!({})),
        &mut events,
    )
    .await;
    assert_eq!(began["ok"], true);
    assert_eq!(
        began["data"]["session"]["target_available"], true,
        "target comes ONLY from the private hyprctl fake"
    );
    let session_id = began["data"]["session"]["id"].as_str().unwrap().to_owned();
    let hidden = || {
        call(
            "hidden",
            "clipboard.session.hidden",
            json!({"session_id":session_id}),
        )
    };
    assert_eq!(
        request(&mut b, hidden(), &mut events).await["ok"],
        false,
        "peer cannot consume hidden authority"
    );
    let paste = call(
        "paste",
        "clipboard.entry.action",
        json!({"entry_id":"fixture", "revision":1, "action":"paste", "session_id":session_id}),
    );
    let prepared = if phase == "paste-prepared" {
        send(&mut a, paste).await;
        serde_json::from_str::<Value>(&marker(&directory, "held").await).unwrap()
    } else {
        request(&mut a, paste, &mut events).await
    };
    assert_eq!(prepared["ok"], true);
    assert_eq!(prepared["data"]["operation"]["status"], "paste-prepared");
    let operation_id = prepared["data"]["operation"]["id"].as_str().unwrap();
    if phase == "paste-hidden" {
        send(&mut a, hidden()).await;
        let held: Value = serde_json::from_str(&marker(&directory, "held").await).unwrap();
        assert_eq!(held["ok"], true);
    } else if phase == "paste-delivered" {
        assert_eq!(request(&mut a, hidden(), &mut events).await["ok"], true);
        timeout(DEADLINE, async {
            while !audit(&directory).iter().any(|row| row["kind"] == "paste") {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            request(&mut a, hidden(), &mut events).await["ok"],
            false,
            "hidden is single-use"
        );
    }
    fixture.process.kill();
    eof(&mut a).await;
    eof(&mut b).await;
    let counts = |rows: &[Value]| {
        (
            rows.iter().filter(|row| row["kind"] == "mutate").count(),
            rows.iter().filter(|row| row["kind"] == "paste").count(),
        )
    };
    let expected = (1, usize::from(phase == "paste-delivered"));
    assert_eq!(
        counts(&audit(&directory)),
        expected,
        "fake publication occurs once; pending hidden work has no rollback guarantee"
    );
    fixture.restart().await;
    let mut fresh = peer(&directory).await;
    assert_eq!(
        status(&mut fresh, operation_id).await["error"]["code"],
        "operation-unknown"
    );
    assert_eq!(
        request(&mut fresh, hidden(), &mut events).await["ok"],
        false,
        "retired session cannot inject input after restart"
    );
    assert_eq!(request(&mut fresh, call("stale-paste", "clipboard.entry.action", json!({"entry_id":"fixture", "revision":1, "action":"paste", "session_id":session_id})), &mut events).await["ok"], false, "stale authority is rejected before publication");
    let new_session = request(
        &mut fresh,
        call("new-begin", "clipboard.session.begin", json!({})),
        &mut events,
    )
    .await;
    assert_eq!(new_session["ok"], true);
    assert_ne!(new_session["data"]["session"]["id"], session_id);
    // A fresh, unarmed session can hide but cannot inherit the old paste intent.
    assert_eq!(
        request(
            &mut fresh,
            call(
                "new-hidden",
                "clipboard.session.hidden",
                json!({"session_id":new_session["data"]["session"]["id"]})
            ),
            &mut events
        )
        .await["ok"],
        true
    );
    tokio::time::sleep(Duration::from_millis(750)).await;
    assert_eq!(
        counts(&audit(&directory)),
        expected,
        "restart/recovery never replays publication or delayed input"
    );
    fixture.process.kill();
    println!("CLIP_PROCESS_CRASH_PASS phase={phase}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_process_owner_cleanup_retains_committing_admission_and_healthy_peers() {
    timeout(Duration::from_secs(40), async {
        let mut fixture = ProcessFixture::new("admission").await;
        let directory = fixture.runtime.clone();
        let mut peers = Vec::new();
        let mut ids = Vec::new();
        for _ in 0..4 {
            let mut connection = peer(&directory).await;
            let mut owned = Vec::new();
            for _ in 0..16 {
                owned.push(capture(&mut connection).await);
            }
            assert_eq!(
                request(
                    &mut connection,
                    call(
                        "busy",
                        "clipboard.capture.interactive",
                        json!({"mode":"region"})
                    ),
                    &mut Vec::new()
                )
                .await["error"]["code"],
                "busy"
            );
            peers.push(connection);
            ids.push(owned);
        }
        drop(peers.remove(0));
        timeout(DEADLINE, async {
            while audit(&directory)
                .iter()
                .filter(|row| row["kind"] == "cancel")
                .count()
                < 16
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        let mut replacement = peer(&directory).await;
        assert_eq!(
            request(
                &mut replacement,
                call(
                    "global-busy",
                    "clipboard.capture.interactive",
                    json!({"mode":"region"})
                ),
                &mut Vec::new()
            )
            .await["error"]["code"],
            "busy",
            "disconnect is cancellation, not rollback or released admission"
        );
        assert_eq!(
            status(&mut peers[0], &ids[1][0]).await["data"]["operation"]["status"],
            "started"
        );
        assert_eq!(
            status(&mut replacement, &ids[0][0]).await["error"]["code"],
            "operation-unknown"
        );
        complete(&directory, &ids[0]).await;
        let new_id = capture(&mut replacement).await;
        assert_eq!(
            status(&mut replacement, &new_id).await["data"]["operation"]["status"],
            "started"
        );
        assert_eq!(
            status(&mut peers[0], &ids[1][0]).await["data"]["operation"]["status"],
            "started"
        );
        fixture.process.kill();
    })
    .await
    .unwrap();
}
