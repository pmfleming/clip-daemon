//! Disposable real-daemon rehearsal (never connects to the user's session bus).
//! Build: cargo build --bin clip-daemon --example turso_acceptance --example acceptance-offer
//! Run: cargo run --example turso_acceptance -- --binary target/debug/clip-daemon --nested
//! Omit --nested for a headless compositor; --package OUT checks installed wiring and
//! activates the installed daemon through a private bus. Failed runs retain private logs.
use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::Write,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const BUS: &str = "org.laufan.ClipDaemon";
const OBJECT: &str = "/org/laufan/ClipDaemon";
const INTERFACE: &str = "org.laufan.ClipDaemon2";
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "target/debug/clip-daemon")]
    binary: PathBuf,
    #[arg(long, default_value = "target/debug/examples/acceptance-offer")]
    offer: PathBuf,
    /// Installed package prefix: also exercise its real D-Bus activation file.
    #[arg(long)]
    package: Option<PathBuf>,
    /// Nest only the test compositor on the current Wayland display.
    #[arg(long)]
    nested: bool,
    #[arg(long)]
    keep: bool,
}

pub fn inspect_package(root: &Path) -> Result<PathBuf> {
    let binary = root.join("bin/clip-daemon");
    ensure!(binary.is_file(), "missing {}", binary.display());
    let units = root.join("share/systemd/user");
    let names = fs::read_dir(&units)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        names == ["clip-daemon.service"],
        "package must install exactly one unit: {names:?}"
    );
    let unit = fs::read_to_string(units.join("clip-daemon.service"))?;
    let services = root.join("share/dbus-1/services");
    let names = fs::read_dir(&services)?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        names == ["org.laufan.ClipDaemon.service"],
        "unexpected activation services"
    );
    let activation = fs::read_to_string(services.join("org.laufan.ClipDaemon.service"))?;
    for text in [&unit, &activation] {
        // Stop-only Conflicts/After may name retired units during fresh cutover;
        // they must never be installed, started or required by this package.
        for line in text.lines().map(str::trim) {
            if let Some((key, value)) = line.split_once('=')
                && matches!(
                    key,
                    "Wants"
                        | "Requires"
                        | "Requisite"
                        | "BindsTo"
                        | "Exec"
                        | "ExecStart"
                        | "ExecStartPre"
                        | "ExecStartPost"
                )
            {
                ensure!(
                    !value.to_lowercase().contains("ringboard"),
                    "obsolete service dependency"
                );
            }
        }
        ensure!(
            !text.contains("@out@") && !text.contains("@ringboard@"),
            "unrendered package"
        );
        ensure!(
            !text.contains("[DEFAULT]"),
            "unit commands cannot inherit defaults"
        );
    }
    ensure!(
        unit.lines()
            .any(|line| line == format!("ExecStart={} daemon", binary.display())),
        "unit selects wrong binary"
    );
    ensure!(
        activation
            .lines()
            .any(|line| line == format!("Exec={} daemon", binary.display())),
        "activation selects wrong binary"
    );
    ensure!(
        unit.lines().any(|line| line == format!("BusName={BUS}")),
        "wrong unit bus name"
    );
    ensure!(
        activation.lines().any(|line| line == format!("Name={BUS}")),
        "wrong activation bus name"
    );
    Ok(binary)
}

fn tool(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join(name))
        .find(|p| p.is_file())
        .with_context(|| format!("required tool {name} is unavailable; enter the dev shell"))
}

// Each long-lived process has a private process group. Never reap the leader before
// group cleanup: retaining it prevents PID reuse from authorizing a foreign signal.
struct Process {
    child: Child,
    kill: PathBuf,
}
impl Process {
    fn signal(&self, signal: &str) {
        let _ = Command::new(&self.kill)
            .args([signal, "--", &format!("-{}", self.child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    fn graceful(mut self) -> Result<()> {
        self.signal("-TERM");
        // Shutdown gets its full service deadline; no reaping before group cleanup.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let stat = fs::read_to_string(format!("/proc/{}/stat", self.child.id()))?;
            if stat
                .rsplit_once(") ")
                .is_some_and(|(_, tail)| tail.starts_with('Z'))
            {
                break;
            }
            ensure!(
                Instant::now() < deadline,
                "daemon did not shut down within 30 seconds"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        self.signal("-KILL");
        let status = self.child.wait()?;
        // Drop must not signal a reaped PID.
        self.kill = PathBuf::new();
        ensure!(status.success(), "daemon shutdown failed: {status}");
        Ok(())
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        if !self.kill.as_os_str().is_empty() {
            self.signal("-KILL");
            let _ = self.child.wait();
        }
    }
}

struct Sandbox {
    root: PathBuf,
    env: BTreeMap<String, String>,
    kill: PathBuf,
    timeout: PathBuf,
}
impl Sandbox {
    fn new(root: PathBuf) -> Result<Self> {
        let mut env = BTreeMap::new();
        for (key, dir) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "r"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_DIRS", "share"),
            ("XDG_CONFIG_DIRS", "etc"),
            ("TMPDIR", "tmp"),
        ] {
            let path = root.join(dir);
            fs::create_dir_all(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            env.insert(key.into(), path.to_string_lossy().into_owned());
        }
        // Executable search is the only inherited environment setting. In particular
        // no host bus, socket, service manager, preload or clipboard variables survive.
        env.insert("PATH".into(), std::env::var("PATH")?);
        env.insert("LANG".into(), "C.UTF-8".into());
        env.insert("RUST_LOG".into(), "clip_daemon=info".into());
        Ok(Self {
            root,
            env,
            kill: tool("kill")?,
            timeout: tool("timeout")?,
        })
    }
    fn command(&self, binary: &Path) -> Command {
        let mut command = Command::new(binary);
        command
            .env_clear()
            .envs(&self.env)
            .current_dir(&self.root)
            .stdin(Stdio::null());
        command
    }
    fn spawn(
        &self,
        name: &str,
        binary: &Path,
        args: &[&str],
        input: Option<&[u8]>,
    ) -> Result<Process> {
        let log = File::create(self.root.join(format!("{name}.log")))?;
        let mut command = self.command(binary);
        command
            .args(args)
            .process_group(0)
            .stdout(log.try_clone()?)
            .stderr(log);
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut process = Process {
            child: command.spawn().with_context(|| format!("start {name}"))?,
            kill: self.kill.clone(),
        };
        if let Some(input) = input {
            process.child.stdin.take().unwrap().write_all(input)?;
        }
        Ok(process)
    }
    fn run(&self, binary: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
        // Fixtures are small. File-backed output and input cannot deadlock on pipe
        // capacity; timeout bounds even clients that wait for a missing selection.
        let input_path = self.root.join("command.stdin");
        fs::write(&input_path, input)?;
        let output_path = self.root.join("command.stdout");
        let error_path = self.root.join("command.stderr");
        let status = self
            .command(&self.timeout)
            .args(["--kill-after=2", "20"])
            .arg(binary)
            .args(args)
            .stdin(File::open(input_path)?)
            .stdout(File::create(&output_path)?)
            .stderr(File::create(&error_path)?)
            .status()?;
        ensure!(
            status.success(),
            "{} failed ({status}); see {}",
            binary.display(),
            error_path.display()
        );
        Ok(fs::read(output_path)?)
    }
}

async fn wait_socket(path: &Path) -> Result<()> {
    let end = Instant::now() + DEADLINE;
    while !fs::metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
        ensure!(
            Instant::now() < end,
            "socket {} not ready; inspect private logs",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    Ok(())
}

struct Api {
    connection: zbus::Connection,
    owner: String,
}
impl Api {
    async fn connect(sandbox: &Sandbox, expected: Option<u32>) -> Result<Self> {
        let connection =
            zbus::connection::Builder::address(sandbox.env["DBUS_SESSION_BUS_ADDRESS"].as_str())?
                .build()
                .await?;
        let bus = zbus::fdo::DBusProxy::new(&connection).await?;
        let end = Instant::now() + DEADLINE;
        let name = zbus::names::BusName::try_from(BUS)?;
        loop {
            if let Ok(owner) = bus.get_name_owner(name.clone()).await {
                let pid = bus
                    .get_connection_unix_process_id(owner.clone().into())
                    .await?;
                if let Some(expected) = expected {
                    ensure!(pid == expected, "unexpected daemon owner");
                }
                let environ = fs::read(format!("/proc/{pid}/environ"))?;
                let home = format!("HOME={}", sandbox.env["HOME"]);
                ensure!(
                    environ.split(|b| *b == 0).any(|v| v == home.as_bytes()),
                    "daemon escaped private HOME"
                );
                return Ok(Self {
                    connection,
                    owner: owner.to_string(),
                });
            }
            ensure!(
                Instant::now() < end,
                "daemon did not acquire private bus name"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    async fn response(&self, method: &str, params: Value) -> Result<Value> {
        let proxy =
            zbus::Proxy::new(&self.connection, self.owner.as_str(), OBJECT, INTERFACE).await?;
        let wire: String = tokio::time::timeout(
            DEADLINE,
            proxy.call("Call", &(format!("clipboard.{method}"), params.to_string())),
        )
        .await??;
        Ok(serde_json::from_str(&wire)?)
    }
    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let mut response = self.response(method, params).await?;
        ensure!(response["ok"] == true, "{method} failed: {response}");
        Ok(response["data"].take())
    }
    async fn history(&self, query: &str) -> Result<Value> {
        let end = Instant::now() + DEADLINE;
        loop {
            let mut response = self
                .response("history.query", json!({"query":query,"limit":200}))
                .await?;
            if response["ok"] == true {
                return Ok(response["data"]["history"].take());
            }
            // Capture can invalidate a read snapshot. Retry only this side-effect-free
            // query, never a mutation/publication whose commit may already have won.
            ensure!(
                response["error"]["code"] == "stale-history" && Instant::now() < end,
                "history.query failed: {response}"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    async fn entry(&self, preview: &str) -> Result<Value> {
        let end = Instant::now() + DEADLINE;
        loop {
            let history = self.history("").await?;
            if let Some(entry) = history["entries"]
                .as_array()
                .context("entries")?
                .iter()
                .find(|e| e["preview"] == preview)
            {
                return Ok(entry.clone());
            }
            ensure!(Instant::now() < end, "synthetic capture not visible");
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }
    async fn unchanged(&self, before: &Value) -> Result<()> {
        // Multiple samples, rather than an immediate read that could miss a late write.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            ensure!(
                self.history("").await?["entries"] == *before,
                "excluded selection changed history"
            );
        }
        Ok(())
    }
    async fn pause(&self, paused: bool, private: bool) -> Result<()> {
        self.call(
            "capture.setPaused",
            json!({"paused":paused,"private_mode":private}),
        )
        .await?;
        let state = self.call("settings.get", json!({})).await?;
        ensure!(
            state["capture"]["verified"] == true && state["capture"]["paused"] == paused,
            "unverified capture barrier: {state}"
        );
        Ok(())
    }
    async fn action(&self, entry: &Value, action: &str) -> Result<Value> {
        self.call(
            "entry.action",
            json!({"entry_id":entry["id"],"revision":entry["revision"],"action":action}),
        )
        .await
    }
}

// Discover only beneath the disposable runtime directory, never `hyprctl instances`
// on the user's runtime. The daemon inherits this exact instance for session paste.
async fn connect_hyprland(sandbox: &mut Sandbox) -> Result<()> {
    let end = Instant::now() + DEADLINE;
    loop {
        if let Ok(entries) = fs::read_dir(sandbox.root.join("r/hypr")) {
            for entry in entries.flatten() {
                if fs::metadata(entry.path().join(".socket.sock"))
                    .is_ok_and(|m| m.file_type().is_socket())
                {
                    sandbox.env.insert(
                        "HYPRLAND_INSTANCE_SIGNATURE".into(),
                        entry.file_name().to_string_lossy().into_owned(),
                    );
                    return Ok(());
                }
            }
        }
        ensure!(Instant::now() < end, "private Hyprland IPC not ready");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn hypr_json(sandbox: &Sandbox, query: &str) -> Result<Value> {
    Ok(serde_json::from_slice(&sandbox.run(
        &tool("hyprctl")?,
        &["-j", query],
        &[],
    )?)?)
}

async fn prepare_output(sandbox: &Sandbox) -> Result<()> {
    // A nested Wayland socket may exist before its parent-backed output is usable.
    // Give native UI fixtures a deterministic output, independent of host focus,
    // minimization and configure timing. This is still the real private compositor.
    let output = sandbox.run(
        &tool("hyprctl")?,
        &["output", "create", "headless", "CLIP-ACCEPTANCE"],
        &[],
    )?;
    ensure!(
        output.trim_ascii() == b"ok",
        "create private output: {}",
        String::from_utf8_lossy(&output)
    );
    let end = Instant::now() + DEADLINE;
    loop {
        let monitors = hypr_json(sandbox, "monitors")?;
        if monitors
            .as_array()
            .context("monitors")?
            .iter()
            .any(|m| m["name"] == "CLIP-ACCEPTANCE" && m["width"].as_u64().is_some_and(|n| n > 0))
        {
            // Parent-backed outputs can asynchronously resize or steal the
            // private seat's focus. Keep only the deterministic virtual output.
            for monitor in monitors.as_array().unwrap() {
                if let Some(name) = monitor["name"].as_str()
                    && name != "CLIP-ACCEPTANCE"
                {
                    let response = sandbox.run(
                        &tool("hyprctl")?,
                        &["keyword", "monitor", &format!("{name},disable")],
                        &[],
                    )?;
                    ensure!(
                        response.trim_ascii() == b"ok",
                        "disable private parent output failed"
                    );
                }
            }
            dispatch(sandbox, "focusmonitor", "CLIP-ACCEPTANCE")?;
            fs::write(
                sandbox.root.join("monitors.json"),
                serde_json::to_vec_pretty(&hypr_json(sandbox, "monitors")?)?,
            )?;
            return Ok(());
        }
        ensure!(
            Instant::now() < end,
            "private UI output did not become ready"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

fn dispatch(sandbox: &Sandbox, dispatcher: &str, argument: &str) -> Result<()> {
    let output = sandbox.run(&tool("hyprctl")?, &["dispatch", dispatcher, argument], &[])?;
    ensure!(
        output.trim_ascii() == b"ok",
        "Hyprland {dispatcher}: {}",
        String::from_utf8_lossy(&output)
    );
    Ok(())
}

async fn window(sandbox: &Sandbox, title: Option<&str>) -> Result<Value> {
    let end = Instant::now() + DEADLINE;
    loop {
        let clients = hypr_json(sandbox, "clients")?;
        if let Some(client) = clients
            .as_array()
            .context("Hyprland clients")?
            .iter()
            .find(|c| {
                c["mapped"] == true
                    && c["visible"] == true
                    && c["monitor"].as_i64().is_some_and(|m| m >= 0)
                    && title.map_or_else(
                        || {
                            c["class"]
                                .as_str()
                                .is_some_and(|s| s.to_lowercase().contains("satty"))
                        },
                        |title| c["title"] == title,
                    )
            })
        {
            return Ok(client.clone());
        }
        ensure!(
            Instant::now() < end,
            "fixture window {title:?} did not appear"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn focus(sandbox: &Sandbox, client: &Value) -> Result<()> {
    let address = client["address"].as_str().context("window address")?;
    dispatch(sandbox, "focuswindow", &format!("address:{address}"))?;
    let end = Instant::now() + DEADLINE;
    while hypr_json(sandbox, "activewindow")?["address"] != address {
        ensure!(Instant::now() < end, "fixture window did not acquire focus");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    Ok(())
}

async fn targeted_paste(sandbox: &mut Sandbox, api: &Api, entry: &Value) -> Result<()> {
    let quickshell = tool("quickshell")?;
    let spawn_target = |sandbox: &mut Sandbox, name: &str| -> Result<Process> {
        let fixture = sandbox.root.join(format!("{name}.qml"));
        fs::write(&fixture, include_str!("../test_support/paste-target.qml"))?;
        sandbox
            .env
            .insert("CLIP_ACCEPTANCE_TITLE".into(), name.into());
        sandbox.env.insert(
            "CLIP_ACCEPTANCE_RECEIVED".into(),
            sandbox.root.join(name).to_string_lossy().into(),
        );
        sandbox.spawn(
            name,
            &quickshell,
            &["--path", fixture.to_str().unwrap()],
            None,
        )
    };
    let target = spawn_target(sandbox, "paste-target")?;
    let recipient = window(sandbox, Some("paste-target")).await?;
    focus(sandbox, &recipient).await?;
    let session = api.call("session.begin", json!({})).await?["session"].take();
    ensure!(
        session["target_available"] == true,
        "no private paste target: {session}"
    );
    let decoy = spawn_target(sandbox, "paste-decoy")?;
    let other = window(sandbox, Some("paste-decoy")).await?;
    focus(sandbox, &other).await?;
    // Refresh after publication echoes/window setup, before the one-shot mutation.
    // Never retry an action whose clipboard/session side effects may have won.
    let refreshed = api
        .entry(entry["preview"].as_str().context("text preview")?)
        .await?;
    ensure!(
        refreshed["id"] == entry["id"],
        "paste entry identity changed"
    );
    let prepared = api
        .call(
            "entry.action",
            json!({
                "entry_id":refreshed["id"], "revision":refreshed["revision"],
                "action":"paste", "session_id":session["id"]
            }),
        )
        .await?;
    ensure!(
        prepared["operation"]["status"] == "paste-prepared",
        "paste not armed: {prepared}"
    );
    tokio::time::sleep(Duration::from_millis(800)).await;
    for name in ["paste-target", "paste-decoy"] {
        ensure!(
            fs::read(sandbox.root.join(name))
                .unwrap_or_default()
                .is_empty(),
            "paste injected before hidden handshake"
        );
    }
    let params = json!({"session_id":session["id"]});
    let hidden = api.call("session.hidden", params.clone()).await?;
    let finished = api.call("session.wait", params.clone()).await?;
    fs::write(
        sandbox.root.join("paste-receipt.json"),
        serde_json::to_vec_pretty(&json!({
            "session":session, "prepared":prepared, "hidden":hidden,
            "finished":finished, "target":recipient, "decoy":other
        }))?,
    )?;
    let end = Instant::now() + DEADLINE;
    let expected = entry["preview"]
        .as_str()
        .context("text preview")?
        .as_bytes();
    while fs::read(sandbox.root.join("paste-target")).unwrap_or_default() != expected {
        ensure!(
            Instant::now() < end,
            "target did not receive exact paste bytes"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    ensure!(
        api.response("session.hidden", params.clone()).await?["ok"] == false,
        "hidden handshake replay accepted"
    );
    api.call("session.end", params).await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    ensure!(
        fs::read(sandbox.root.join("paste-target"))? == expected,
        "paste replayed"
    );
    ensure!(
        fs::read(sandbox.root.join("paste-decoy"))
            .unwrap_or_default()
            .is_empty(),
        "paste reached the wrong window"
    );
    drop(decoy);
    drop(target);
    sandbox.env.remove("CLIP_ACCEPTANCE_TITLE");
    sandbox.env.remove("CLIP_ACCEPTANCE_RECEIVED");
    println!(
        "PASS targeted QML paste: hidden barrier, original-window handoff, exact bytes, no replay/decoy input"
    );
    Ok(())
}

async fn annotation(sandbox: &Sandbox, api: &Api, paste: &Path, original: &[u8]) -> Result<()> {
    tool("satty")?;
    let mut selected = api.history("").await?["entries"]
        .as_array()
        .context("entries")?
        .iter()
        .find(|e| e["kind"] == "image")
        .context("image entry")?
        .clone();
    for mode in ["save", "escape", "cancel"] {
        let before = api.history("").await?["entries"].take();
        selected = before
            .as_array()
            .context("entries")?
            .iter()
            .find(|e| e["id"] == selected["id"])
            .context("annotation entry disappeared")?
            .clone();
        let clipboard = sandbox.run(paste, &["--type", "image/png"], &[])?;
        let started = api.action(&selected, "annotate").await?;
        ensure!(
            started["operation"]["status"] == "started",
            "annotation did not start: {started}"
        );
        let id = started["operation"]["id"]
            .as_str()
            .context("operation id")?;
        let editor = window(sandbox, None).await?;
        focus(sandbox, &editor).await?;
        // Mapping precedes GTK's first rendered frame/input readiness. Allow the
        // software renderer to settle, then reassert focus immediately before the
        // one-shot shortcut (never retry an ambiguous save/cancel input).
        tokio::time::sleep(Duration::from_millis(1500)).await;
        focus(sandbox, &editor).await?;
        if mode == "cancel" {
            let proxy =
                zbus::Proxy::new(&api.connection, api.owner.as_str(), OBJECT, INTERFACE).await?;
            let wire: String =
                tokio::time::timeout(DEADLINE, proxy.call("Cancel", &(id))).await??;
            let response: Value = serde_json::from_str(&wire)?;
            ensure!(
                response["ok"] == true,
                "active editor cancellation failed: {response}"
            );
        } else {
            let key = if mode == "save" { "Return" } else { "Escape" };
            dispatch(
                sandbox,
                "sendshortcut",
                &format!(",{key},address:{}", editor["address"].as_str().unwrap()),
            )?;
        }
        let end = Instant::now() + DEADLINE;
        let terminal = loop {
            let status = api
                .call("operation.status", json!({"operation_id":id}))
                .await?;
            if status["operation"]["status"] != "started" {
                break status;
            }
            ensure!(
                Instant::now() < end,
                "Satty {mode} did not terminate: {status}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        };
        fs::write(
            sandbox.root.join(format!("satty-{mode}-receipt.json")),
            serde_json::to_vec_pretty(&terminal)?,
        )?;
        while hypr_json(sandbox, "clients")?
            .as_array()
            .context("clients")?
            .iter()
            .any(|c| c["address"] == editor["address"])
        {
            ensure!(Instant::now() < end, "Satty {mode} left its window running");
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let result = &terminal["operation"];
        if mode == "save" {
            ensure!(
                result["status"] == "completed" && result["warning"].is_null(),
                "Satty save failed: {terminal}"
            );
            let history = api.history("").await?;
            let saved = history["entries"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["id"] == selected["id"])
                .context("annotation lost stable identity")?
                .clone();
            ensure!(
                saved["revision"] != selected["revision"],
                "annotation did not advance revision"
            );
            let bytes = sandbox.run(paste, &["--type", "image/png"], &[])?;
            ensure!(
                image::load_from_memory(&bytes)?.to_rgba8()
                    == image::load_from_memory(original)?.to_rgba8(),
                "Satty save changed unedited pixels"
            );
            fs::write(sandbox.root.join("satty-saved.png"), &bytes)?;
            api.action(&saved, "copy").await?;
            ensure!(
                sandbox.run(paste, &["--type", "image/png"], &[])? == bytes,
                "stored annotation differs from published bytes"
            );
            selected = saved;
        } else {
            let expected = if mode == "cancel" {
                "cancelled"
            } else {
                "failed"
            };
            ensure!(
                result["status"] == expected
                    && result["message"]
                        .as_str()
                        .is_some_and(|s| s.contains("cancelled")),
                "unexpected cancellation receipt: {terminal}"
            );
            api.unchanged(&before).await?;
            ensure!(
                sandbox.run(paste, &["--type", "image/png"], &[])? == clipboard,
                "cancelled editor changed clipboard bytes"
            );
        }
        println!(
            "PASS real Satty {mode}: terminal receipt, editor exit, clipboard/history integrity"
        );
    }
    Ok(())
}

async fn rehearse(args: &Args, sandbox: &mut Sandbox) -> Result<()> {
    let binary = fs::canonicalize(if let Some(package) = &args.package {
        inspect_package(&fs::canonicalize(package)?)?
    } else {
        args.binary.clone()
    })?;
    let offer = fs::canonicalize(&args.offer)?;
    let compositor_binary = tool("Hyprland")?;
    let copy = tool("wl-copy")?;
    let paste = tool("wl-paste")?;
    // An empty configuration lets the compositor choose its default output mode.
    // No generated Lua configuration or executable scripts are needed.
    let config = sandbox.root.join("hyprland.conf");
    fs::write(&config, "")?;
    sandbox
        .env
        .insert("AQ_DRM_DEVICES".into(), "/dev/null".into());
    sandbox
        .env
        .insert("LIBGL_ALWAYS_SOFTWARE".into(), "1".into());
    let parent = if args.nested {
        let display = PathBuf::from(
            std::env::var("WAYLAND_DISPLAY")
                .context("--nested requires a parent Wayland display")?,
        );
        Some(if display.is_absolute() {
            display
        } else {
            PathBuf::from(std::env::var("XDG_RUNTIME_DIR")?).join(display)
        })
    } else {
        None
    };
    if let Some(parent) = &parent {
        sandbox
            .env
            .insert("WAYLAND_DISPLAY".into(), parent.to_string_lossy().into());
    } else {
        sandbox.env.insert("AQ_BACKEND".into(), "headless".into());
    }
    let compositor_args = ["--config", config.to_str().unwrap()];
    let compositor = sandbox.spawn("compositor", &compositor_binary, &compositor_args, None)?;
    let end = Instant::now() + DEADLINE;
    let socket = loop {
        if let Some(socket) = fs::read_dir(sandbox.root.join("r"))?
            .filter_map(Result::ok)
            .find(|entry| {
                entry.file_name().to_string_lossy().starts_with("wayland-")
                    && entry.file_type().is_ok_and(|kind| kind.is_socket())
            })
        {
            break socket.path();
        }
        ensure!(
            Instant::now() < end,
            "compositor socket not ready; inspect private logs"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    let display = socket.file_name().unwrap().to_string_lossy().into_owned();
    sandbox
        .env
        .insert("WAYLAND_DISPLAY".into(), display.clone());

    connect_hyprland(sandbox).await?;
    prepare_output(sandbox).await?;
    sandbox
        .env
        .insert("QT_QPA_PLATFORM".into(), "wayland".into());
    sandbox.env.insert("GDK_BACKEND".into(), "wayland".into());
    // Keep native GTK fixtures independent of host GPU/Vulkan initialization and
    // accessibility services, which are deliberately absent from the private bus.
    sandbox.env.insert("GSK_RENDERER".into(), "cairo".into());
    sandbox.env.insert("GTK_A11Y".into(), "none".into());

    let services = sandbox.root.join("services");
    fs::create_dir(&services)?;
    if let Some(package) = &args.package {
        fs::copy(
            package.join("share/dbus-1/services/org.laufan.ClipDaemon.service"),
            services.join("org.laufan.ClipDaemon.service"),
        )?;
    }
    let bus_path = sandbox.root.join("r/bus");
    let bus_config = sandbox.root.join("bus.conf");
    // No standard service directories, systemd activation or host configuration.
    fs::write(
        &bus_config,
        format!(
            "<busconfig><type>session</type><listen>unix:path={}</listen><servicedir>{}</servicedir><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
            bus_path.display(),
            services.display()
        ),
    )?;
    sandbox.env.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={}", bus_path.display()),
    );
    let _bus = sandbox.spawn(
        "bus",
        &tool("dbus-daemon")?,
        &["--nofork", "--config-file", bus_config.to_str().unwrap()],
        None,
    )?;
    wait_socket(&bus_path).await?;
    let mut daemon = if args.package.is_none() {
        Some(sandbox.spawn("daemon", &binary, &["daemon"], None)?)
    } else {
        let connection =
            zbus::connection::Builder::address(sandbox.env["DBUS_SESSION_BUS_ADDRESS"].as_str())?
                .build()
                .await?;
        let proxy = zbus::fdo::DBusProxy::new(&connection).await?;
        tokio::time::timeout(
            DEADLINE,
            proxy.start_service_by_name(zbus::names::WellKnownName::try_from(BUS)?, 0),
        )
        .await??;
        None
    };
    let mut api = Api::connect(sandbox, daemon.as_ref().map(|d| d.child.id())).await?;
    ensure!(
        api.history("").await?["entries"] == json!([]),
        "refusing nonempty initial history"
    );
    let initial = api.call("settings.get", json!({})).await?;
    ensure!(
        initial["capture"]["desired_paused"] == true,
        "missing intent must start paused"
    );
    api.pause(false, false).await?;
    api.call("settings.update", json!({"max_entry_bytes":65536}))
        .await?;
    println!("PASS isolated empty store, fail-closed defaults and direct policy update");

    let started = Instant::now();
    let _producer = sandbox.spawn(
        "external-copy",
        &copy,
        &["--foreground", "--type", "text/plain"],
        Some(b"acceptance orchard"),
    )?;
    let entry = api.entry("acceptance orchard").await?;
    println!(
        "PASS external capture ({} ms)",
        started.elapsed().as_millis()
    );
    ensure!(
        api.history("orchard").await?["total"] == 1,
        "FTS query missed captured text"
    );
    api.action(&entry, "favorite").await?;
    let favorite = api.entry("acceptance orchard").await?;
    ensure!(favorite["favorite"] == true, "favorite did not persist");
    let lease = api
        .call(
            "entry.edit.begin",
            json!({"entry_id":favorite["id"],"revision":favorite["revision"]}),
        )
        .await?;
    api.call(
        "entry.edit.commit",
        json!({"edit_id":lease["edit"]["id"],"value":"acceptance edited orchard"}),
    )
    .await?;
    let edited = api.entry("acceptance edited orchard").await?;
    ensure!(
        edited["id"] == entry["id"]
            && edited["favorite"] == true
            && edited["revision"] != favorite["revision"],
        "edit lost identity/favorite/revision"
    );
    api.action(&edited, "copy").await?;
    ensure!(
        sandbox.run(&paste, &["--no-newline", "--type", "text/plain"], &[])?
            == b"acceptance edited orchard",
        "copy bytes changed"
    );
    let before = api.history("").await?["entries"].take();
    api.unchanged(&before).await?;
    println!("PASS query, favorite, edit, exact copy and echo exclusion");
    targeted_paste(sandbox, &api, &edited).await?;

    sandbox.run(
        &binary,
        &["publish", "--mime", "text/plain"],
        b"acceptance cli publication",
    )?;
    api.entry("acceptance cli publication").await?;
    let request = json!({"op":"call","id":"acceptance","method":"clipboard.history.query","params":{"query":"orchard"}});
    let wire = sandbox.run(&binary, &["client"], format!("{request}\n").as_bytes())?;
    ensure!(
        String::from_utf8(wire)?
            .lines()
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .any(|r| r["id"] == "acceptance" && r["response"]["ok"] == true),
        "JSONL CLI did not return success"
    );
    println!("PASS binary publish and JSONL client");

    let before = api.history("").await?["entries"].take();
    api.pause(true, false).await?;
    let _paused = sandbox.spawn(
        "paused-copy",
        &copy,
        &["--foreground", "--type", "text/plain"],
        Some(b"excluded paused selection"),
    )?;
    api.unchanged(&before).await?;
    api.pause(false, false).await?;
    api.unchanged(&before).await?;
    api.pause(true, true).await?;
    let _private = sandbox.spawn(
        "private-copy",
        &copy,
        &["--foreground", "--type", "text/plain"],
        Some(b"excluded private selection"),
    )?;
    api.unchanged(&before).await?;
    // Persist private intent across a graceful checkpoint/restart with selection held.
    if let Some(old) = daemon.take() {
        drop(api);
        old.graceful()?;
        daemon = Some(sandbox.spawn("daemon-restarted", &binary, &["daemon"], None)?);
        api = Api::connect(sandbox, daemon.as_ref().map(|d| d.child.id())).await?;
        ensure!(
            api.call("settings.get", json!({})).await?["capture"]["private_mode"] == true,
            "private intent lost on restart"
        );
        api.unchanged(&before).await?;
        println!("PASS graceful restart durability and persisted private startup");
    } else {
        println!("NOT RUN restart: installed activation mode (run --binary for restart rehearsal)");
    }
    api.pause(false, false).await?;
    api.unchanged(&before).await?;
    let _secret = sandbox.spawn(
        "sensitive-offer",
        &offer,
        &["--sensitive"],
        Some(b"excluded sensitive offer"),
    )?;
    let end = Instant::now() + DEADLINE;
    while !String::from_utf8(sandbox.run(&paste, &["--list-types"], &[])?)?
        .contains("x-kde-passwordManagerHint")
    {
        ensure!(Instant::now() < end, "sensitive producer not selected");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    api.unchanged(&before).await?;
    api.call(
        "selection.publishSensitive",
        json!({"text":"excluded sensitive API","expiry_seconds":30}),
    )
    .await?;
    ensure!(
        sandbox.run(&paste, &["--no-newline", "--type", "text/plain"], &[])?
            == b"excluded sensitive API",
        "sensitive publication missing"
    );
    api.unchanged(&before).await?;
    println!("PASS pause/private barriers, resume skip and sensitive offer/API exclusion");

    let path = sandbox.root.join("fixture.txt");
    fs::write(&path, "synthetic fixture")?;
    api.call(
        "selection.publishFiles",
        json!({"operation":"cut","paths":[path]}),
    )
    .await?;
    let uri = format!("{}\r\n", url::Url::from_file_path(&path).unwrap());
    ensure!(
        sandbox.run(&paste, &["--no-newline", "--type", "text/uri-list"], &[])? == uri.as_bytes(),
        "URI MIME mismatch"
    );
    ensure!(
        sandbox.run(
            &paste,
            &["--no-newline", "--type", "x-special/gnome-copied-files"],
            &[]
        )? == format!("cut\n{uri}").as_bytes(),
        "cut MIME mismatch"
    );
    println!("PASS file publication MIME aliases");

    let mut png = std::io::Cursor::new(Vec::new());
    image::DynamicImage::new_rgb8(320, 240).write_to(&mut png, image::ImageFormat::Png)?;
    let png = png.into_inner();
    sandbox.run(&binary, &["publish", "--mime", "image/png"], &png)?;
    ensure!(
        sandbox.run(&paste, &["--type", "image/png"], &[])? == png,
        "image publication bytes changed"
    );
    ensure!(
        api.history("").await?["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "image"),
        "image history missing"
    );
    annotation(sandbox, &api, &paste, &png).await?;
    let before = api.history("").await?["entries"].take();
    let _primary = sandbox.spawn(
        "primary-copy",
        &copy,
        &["--foreground", "--primary", "--type", "text/plain"],
        Some(b"excluded primary selection"),
    )?;
    api.unchanged(&before).await?;
    let _oversize = sandbox.spawn(
        "oversize-copy",
        &copy,
        &["--foreground", "--type", "application/octet-stream"],
        Some(&vec![42; 65537]),
    )?;
    api.unchanged(&before).await?;
    let _boundary = sandbox.spawn(
        "boundary-copy",
        &copy,
        &["--foreground", "--type", "application/octet-stream"],
        Some(&vec![42; 65536]),
    )?;
    let end = Instant::now() + DEADLINE;
    loop {
        if api.history("").await?["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["byte_size"] == 65536)
        {
            break;
        }
        ensure!(Instant::now() < end, "exact policy limit was not captured");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    println!("PASS image bytes, primary exclusion and exact capture size boundary");

    let monitors = hypr_json(sandbox, "monitors")?;
    let monitor = monitors
        .as_array()
        .context("screenshot monitors")?
        .iter()
        .find(|m| m["name"] == "CLIP-ACCEPTANCE")
        .context("private screenshot output")?;
    let capture = api
        .call(
            "capture.screenshot",
            json!({"x":monitor["x"],"y":monitor["y"],"width":64,"height":64}),
        )
        .await?;
    ensure!(capture["operation"]["status"] == "completed", "{capture}");
    let screenshot = sandbox.run(&paste, &["--type", "image/png"], &[])?;
    let decoded = image::load_from_memory(&screenshot)?;
    // Region coordinates are logical pixels; grim defaults to the largest
    // output scale (the isolated compositor may inherit a HiDPI default).
    let scale = monitors
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["scale"].as_f64())
        .fold(1.0_f64, f64::max);
    let edge = (64.0 * scale).round() as u32;
    ensure!(
        decoded.width() == edge && decoded.height() == edge,
        "wrong capture dimensions: {}x{} vs {edge}",
        decoded.width(),
        decoded.height()
    );
    let page = api.history("").await?;
    let captured = page["entries"]
        .as_array()
        .context("screenshot history")?
        .iter()
        .find(|e| e["current"] == true && e["kind"] == "image")
        .context("screenshot not stored as current image")?;
    api.action(captured, "copy").await?;
    ensure!(
        sandbox.run(&paste, &["--type", "image/png"], &[])? == screenshot,
        "stored screenshot differs from publication"
    );
    println!("PASS real grim screenshot, dimensions and exact SQL restoration");

    let selected = api.entry("acceptance edited orchard").await?;
    api.action(&selected, "delete").await?;
    ensure!(
        api.history("orchard").await?["total"] == 0,
        "delete left indexed text"
    );
    if let Some(old) = daemon.take() {
        drop(api);
        old.graceful()?;
        daemon = Some(sandbox.spawn("daemon-delete-restarted", &binary, &["daemon"], None)?);
        api = Api::connect(sandbox, daemon.as_ref().map(|d| d.child.id())).await?;
        ensure!(
            api.history("orchard").await?["total"] == 0,
            "deleted entry returned after restart"
        );
        api.entry("acceptance cli publication").await?;
        println!("PASS durable deletion and surviving new history");
    }

    drop(compositor);
    let end = Instant::now() + DEADLINE;
    while api.call("settings.get", json!({})).await?["capture"]["verified"] == true {
        ensure!(
            Instant::now() < end,
            "compositor loss not reflected in health"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if let Some(parent) = parent {
        sandbox
            .env
            .insert("WAYLAND_DISPLAY".into(), parent.to_string_lossy().into());
    } else {
        sandbox.env.remove("WAYLAND_DISPLAY");
    }
    let _reconnected = sandbox.spawn(
        "compositor-reconnected",
        &compositor_binary,
        &compositor_args,
        None,
    )?;
    wait_socket(&socket).await?;
    sandbox.env.insert("WAYLAND_DISPLAY".into(), display);
    let end = Instant::now() + DEADLINE;
    while api.call("settings.get", json!({})).await?["capture"]["verified"] != true {
        ensure!(
            Instant::now() < end,
            "capture did not recover after compositor reconnect"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _after = sandbox.spawn(
        "after-reconnect",
        &copy,
        &["--foreground", "--type", "text/plain"],
        Some(b"acceptance reconnected"),
    )?;
    api.entry("acceptance reconnected").await?;
    println!("PASS compositor health and reconnect capture");
    let selected = api.entry("acceptance reconnected").await?;
    let edit = api
        .call(
            "entry.edit.begin",
            json!({"entry_id":selected["id"],"revision":selected["revision"]}),
        )
        .await?;
    let challenge = api.call("history.wipe.prepare", json!({})).await?;
    api.call(
        "history.wipe.commit",
        json!({"challenge_id":challenge["challenge"]["id"],"response":"WIPE"}),
    )
    .await?;
    ensure!(
        api.response(
            "entry.edit.commit",
            json!({"edit_id":edit["edit"]["id"],"value":"must not resurrect wiped entry"})
        )
        .await?["ok"]
            == false,
        "wipe did not fence editor lease"
    );
    api.unchanged(&json!([])).await?;
    ensure!(path.is_file(), "wipe deleted a user-owned copied file");
    println!("PASS wipe, editor fence, no selection replay and user file preservation");
    if let Some(daemon) = daemon {
        daemon.graceful()?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let directory = tempfile::Builder::new()
        .prefix("clip-at-")
        .tempdir_in("/tmp")?;
    let mut sandbox = Sandbox::new(directory.path().into())?;
    let result = rehearse(&args, &mut sandbox).await;
    if args.keep || result.is_err() {
        eprintln!(
            "Private synthetic fixtures/logs retained at {}",
            directory.keep().display()
        );
    }
    if let Err(error) = result {
        bail!("acceptance failed: {error:#}");
    }
    println!("PASS Turso desktop acceptance");
    Ok(())
}
