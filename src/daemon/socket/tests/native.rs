//! Explicit real-Quickshell acceptance, with private paths and no host activation.
use super::*;
use anyhow::{Context, ensure};
use std::{collections::HashSet, sync::Mutex};

#[derive(Default)]
struct Audit {
    owners: HashSet<String>,
    reads: usize,
    subscriptions: usize,
    cancellations: usize,
    disconnects: usize,
}
struct Probe {
    service: Service,
    api: Arc<ApiService>,
    backend: Arc<FakeBackend>,
    audit: Arc<Mutex<Audit>>,
    large: bool,
}
impl LocalJsonlService for Probe {
    async fn call(&self, session: &SocketSession, method: &str, params: Value) -> Result<Value> {
        ensure!(
            ["clipboard.history.query", "clipboard.history.revision"].contains(&method),
            "private probe attempted an effect"
        );
        let reads = {
            let mut audit = self.audit.lock().unwrap();
            audit.owners.insert(session.owner().into());
            audit.reads += 1;
            audit.reads
        };
        ensure!(reads <= 20, "unexpected recovery loop");
        if self.large && reads == 1 {
            let padding = params["probe_padding"]
                .as_str()
                .context("missing large request")?;
            ensure!(
                padding.len() == 16 * 1024 * 1024 && padding.bytes().all(|byte| byte == 0),
                "large escaped request was damaged"
            );
        }
        if self.large && reads == 2 {
            self.backend
                .mutate("fixture", Some(1), crate::backend::BackendMutation::Remove)
                .await?;
        }
        let response = self.service.call(session, method, params).await?;
        if (self.large && reads == 2) || (!self.large && reads == 8) {
            if !self.large {
                self.backend
                    .mutate("fixture", Some(1), crate::backend::BackendMutation::Remove)
                    .await?;
            }
            let observation = self
                .api
                .history_observation()
                .await
                .map_err(|_| anyhow::anyhow!("fixture history observation failed"))?;
            let interface = self
                .service
                .0
                .object_server()
                .interface::<_, ClipDaemon>(OBJECT_PATH)
                .await?;
            let daemon = interface.get().await;
            let _ =
                daemon
                    .history_events
                    .send(crate::daemon::subscription::HistoryUpdate::Changed(
                        observation,
                    ));
            // Release an older captured read after a genuine native invalidation.
            // Either delivery order must converge without replaying any effect.
            tokio::task::yield_now().await;
        }
        Ok(response)
    }
    async fn subscribe(&self, session: &SocketSession, streams: Vec<String>) -> Result<Value> {
        ensure!(
            streams.iter().all(
                |stream| ["clipboard.history.changed", "clipboard.current.changed"]
                    .contains(&stream.as_str())
            ),
            "unexpected streams"
        );
        {
            let mut audit = self.audit.lock().unwrap();
            audit.owners.insert(session.owner().into());
            audit.subscriptions += 1;
        }
        self.service.subscribe(session, streams).await
    }
    async fn cancel(&self, session: &SocketSession, id: &str) -> Result<Value> {
        self.audit.lock().unwrap().cancellations += 1;
        self.service.cancel(session, id).await
    }
    async fn disconnected(&self, session: &SocketSession) {
        self.audit.lock().unwrap().disconnects += 1;
        self.service.disconnected(session).await;
    }
}

#[tokio::test]
#[ignore = "explicit native-quickshell-socket feature and private probe inputs required"]
async fn native_quickshell_clip_socket_acceptance() -> Result<()> {
    let source = std::path::PathBuf::from(
        std::env::var_os("SHELLLIST_SOURCE").context("SHELLLIST_SOURCE required")?,
    );
    let program =
        std::env::var_os("SHELLLIST_QUICKSHELL").context("SHELLLIST_QUICKSHELL required")?;
    for large in [false, true] {
        native_case(&source, &program, large).await?;
    }
    Ok(())
}

async fn native_case(
    source: &std::path::Path,
    program: &std::ffi::OsStr,
    large: bool,
) -> Result<()> {
    let mut value = entry();
    if large {
        value.entry.preview = "🦀".repeat((config().maximum_response_bytes - 4096) / 4);
    }
    let large_characters = if large {
        value.entry.preview.encode_utf16().count()
    } else {
        0
    };
    let backend = Arc::new(FakeBackend::with_entries(vec![value]));
    let fixture = Fixture::new(backend.clone()).await;
    let runtime = fixture.directory.path().canonicalize()?;
    let listener = JsonlListener::bind(&runtime, "clip-daemon").await?;
    let audit = Arc::new(Mutex::new(Audit::default()));
    let (stop, stopped) = oneshot::channel();
    let serving = tokio::spawn(listener.serve(
        Arc::new(Probe {
            service: Service(fixture.server.clone()),
            api: fixture.api.clone(),
            backend,
            audit: audit.clone(),
            large,
        }),
        crate::client::ClipCorrelation,
        config(),
        async {
            let _ = stopped.await;
        },
    ));
    let home = runtime.join("home");
    std::fs::create_dir(&home)?;
    let child = tokio::process::Command::new(program)
        .args(["--no-color", "--path"])
        .arg(source.join("tests/e2e/clip-unix-jsonl.qml"))
        .env_clear()
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("QML_IMPORT_PATH", source.join("qml"))
        .env("QT_QPA_PLATFORM", "offscreen")
        .env("QT_QUICK_BACKEND", "software")
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            "unix:path=/nonexistent-clip-system",
        )
        .env(
            "DBUS_SESSION_BUS_ADDRESS",
            "unix:path=/nonexistent-clip-session",
        )
        .env("SHELLLIST_CLIP_SOCKET", "1")
        .env("SHELLLIST_PRIVATE_CLIP_PROBE", "1")
        .env(
            "SHELLLIST_CLIP_PROBE_LARGE_CHARACTERS",
            large_characters.to_string(),
        )
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let result = timeout(Duration::from_secs(70), child.wait_with_output()).await;
    // Verify disconnect before listener shutdown, rather than counting shutdown cleanup.
    let disconnected = timeout(DEADLINE, async {
        while audit.lock().unwrap().disconnects == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    fixture.shutdown(stop, serving).await;
    let output = result??;
    let log = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        output.status.success() && log.contains("CLIP_SOCKET_PROBE_PASS"),
        "{log}"
    );
    disconnected?;
    let audit = audit.lock().unwrap();
    ensure!(
        audit.owners.len() == 1
            && audit.subscriptions == 3
            && audit.reads >= 9
            && audit.reads <= 20,
        "unexpected owner/read/subscription counts"
    );
    ensure!(
        audit.cancellations >= 2 && audit.disconnects == 1,
        "missing release cleanup"
    );
    println!("{log}");
    Ok(())
}
