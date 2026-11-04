//! Real engine/process faults. Every database is under a fresh TempDir; no live
//! daemon, default history path, mock storage, or production fault hooks are used.
use clip_daemon::{
    history_store::{HistoryStore, MAX_ENTRY_BYTES, StorePolicy},
    model::{EntrySummary, HistoryQuery},
};
use futures_lite::future::block_on;
use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    os::unix::{fs::MetadataExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

const CHILD_MODE: &str = "CLIP_TURSO_FAULT_CHILD";
const CHILD_ROOT: &str = "CLIP_TURSO_FAULT_ROOT";
const ABRUPT_EXIT: i32 = 73;
const LARGE_BYTES: usize = 16 * 1024 * 1024;
const OLD: &[u8] = b"acknowledged old searchablemarker";
const OTHER: &[u8] = b"independent acknowledged sibling";
const TIMEOUT: Duration = Duration::from_secs(60);

fn query(text: &str) -> HistoryQuery {
    HistoryQuery {
        query: text.into(),
        offset: 0,
        limit: 100,
        generation: 0,
        collapse_self_echoes: false,
    }
}

fn enable(store: &HistoryStore) {
    store
        .apply_policy(StorePolicy {
            admission: true,
            max_entry_bytes: MAX_ENTRY_BYTES,
            ..StorePolicy::default()
        })
        .unwrap();
}

fn large_payload() -> Vec<u8> {
    // Invalid UTF-8 avoids duplicating 16 MiB into the textual search index, but
    // still exercises real BLOB overflow pages and WAL transaction framing.
    (0..LARGE_BYTES)
        .map(|n| ((n * 31 + 255) % 256) as u8)
        .collect()
}

struct Fixture {
    dir: TempDir,
    entry: EntrySummary,
    sibling: EntrySummary,
    revision: u64,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
        enable(&store);
        let entry = store.insert("text/plain", OLD).unwrap();
        let sibling = store.insert("text/plain", OTHER).unwrap();
        let revision = store.revision().unwrap();
        fs::write(dir.path().join("entry-id"), &entry.id).unwrap();
        fs::write(dir.path().join("sibling-id"), &sibling.id).unwrap();
        store.shutdown().unwrap(); // Direct-engine fault children never compete with this owner.
        Self {
            dir,
            entry,
            sibling,
            revision,
        }
    }
    fn path(&self) -> PathBuf {
        self.dir.path().join("history.db")
    }
    fn assert_old(&self, store: &HistoryStore, revision: u64) {
        assert_eq!(store.revision().unwrap(), revision);
        assert_eq!(
            store
                .get(&self.entry.id, Some(self.entry.revision))
                .unwrap()
                .bytes,
            OLD
        );
        assert_eq!(
            store
                .get(&self.sibling.id, Some(self.sibling.revision))
                .unwrap()
                .bytes,
            OTHER
        );
        let page = store.query(query("")).unwrap();
        assert_eq!(page.total, 2);
        assert_eq!(page.entries.len(), 2);
        assert_eq!(page.current.unwrap().id, self.sibling.id);
        let search = store.query(query("searchablemarker")).unwrap();
        assert_eq!(search.entries.len(), 1);
        assert_eq!(search.entries[0].id, self.entry.id);
        assert!(
            store
                .query(query("uncommittedmarker"))
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(!store.policy().unwrap().admission);
    }
}

// Kill/reap even when an assertion fails, so no stranded writer outlives TempDir.
struct FaultChild {
    child: Child,
    log: PathBuf,
}
impl FaultChild {
    fn spawn(mode: &str, root: &Path) -> Self {
        let log = root.join(format!("{mode}.log"));
        let output = File::create(&log).unwrap();
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "fault_subprocess",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_MODE, mode)
            .env(CHILD_ROOT, root)
            // Defense in depth: an accidental default-path call stays isolated.
            .env("HOME", root)
            .env("XDG_DATA_HOME", root.join("data"))
            .stdin(Stdio::null())
            .stdout(output.try_clone().unwrap())
            .stderr(output)
            .spawn()
            .unwrap();
        Self { child, log }
    }
    fn diagnostics(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }
    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "fault child timed out: {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(2));
        }
    }
    fn wait_for(&mut self, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if condition() {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "child exited before fault point: {}",
                self.diagnostics()
            );
            assert!(
                Instant::now() < deadline,
                "fault point timed out: {}",
                self.diagnostics()
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    fn abrupt_exit(&mut self) {
        let status = self.wait();
        assert_eq!(status.code(), Some(ABRUPT_EXIT), "{}", self.diagnostics());
    }
}
impl Drop for FaultChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn marker(root: &Path, name: &str, value: impl ToString) {
    // Atomic rename keeps the supervising process from seeing partial numbers.
    let pending = root.join(format!("{name}.pending"));
    fs::write(&pending, value.to_string()).unwrap();
    fs::rename(pending, root.join(name)).unwrap();
}
fn number(root: &Path, name: &str) -> u64 {
    fs::read_to_string(root.join(name))
        .unwrap()
        .parse()
        .unwrap()
}
fn file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

#[test]
fn initial_schema_survives_exit_without_checkpoint_or_destructors() {
    let dir = tempfile::tempdir().unwrap();
    FaultChild::spawn("initial-schema", dir.path()).abrupt_exit();
    assert!(
        file_len(&dir.path().join("history.db-wal")) > 0,
        "must exercise committed WAL recovery"
    );
    let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    assert_eq!(store.revision().unwrap(), 0);
    assert_eq!(store.query(query("")).unwrap().total, 0);
    assert!(!store.policy().unwrap().admission);
    enable(&store);
    store
        .insert("text/plain", b"schema is usable after recovery")
        .unwrap();
    store.shutdown().unwrap();
}

#[test]
fn acknowledged_commit_survives_process_exit_without_destructors() {
    let dir = tempfile::tempdir().unwrap();
    FaultChild::spawn("committed", dir.path()).abrupt_exit();
    assert!(file_len(&dir.path().join("history.db-wal")) > 0);
    let id = fs::read_to_string(dir.path().join("committed-id")).unwrap();
    let revision = number(dir.path(), "committed-revision");
    let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    assert_eq!(store.revision().unwrap(), revision);
    assert_eq!(store.get(&id, Some(revision)).unwrap().bytes, OLD);
    assert_eq!(
        store.query(query("searchablemarker")).unwrap().entries[0].id,
        id
    );
    assert_eq!(store.query(query("")).unwrap().total, 1);
    assert!(!store.policy().unwrap().admission);
    store.shutdown().unwrap();
}

#[test]
fn uncommitted_sql_transaction_is_discarded_after_process_exit() {
    let fixture = Fixture::new();
    FaultChild::spawn("uncommitted", fixture.dir.path()).abrupt_exit();
    assert!(fixture.dir.path().join("uncommitted-executed").exists());
    let store = HistoryStore::open(&fixture.path()).unwrap();
    fixture.assert_old(&store, fixture.revision);
    store.shutdown().unwrap();
}

#[test]
fn sigkill_during_large_wal_write_recovers_one_whole_transaction() {
    let fixture = Fixture::new();
    let root = fixture.dir.path();
    let mut child = FaultChild::spawn("large-write", root);
    child.wait_for(|| root.join("ready-revision").exists());
    let before = number(root, "ready-revision");
    let wal = root.join("history.db-wal");
    let old_len = file_len(&wal);
    marker(root, "go", "go");
    // Synchronize on actual database IO, not a sleep pretending to hit COMMIT.
    // Public APIs cannot expose the exact commit instruction: this kills during
    // a large transaction's WAL write, possibly just after its commit record.
    child.wait_for(|| file_len(&wal) > old_len + 64 * 1024);
    child.child.kill().unwrap();
    assert_eq!(child.wait().signal(), Some(9));
    let acknowledged = root.join("large-ack").exists();
    let store = HistoryStore::open(&fixture.path()).unwrap();
    let recovered = store.get(&fixture.entry.id, None).unwrap();
    if recovered.bytes == OLD {
        assert!(!acknowledged, "an acknowledged commit must never be lost");
        fixture.assert_old(&store, before);
    } else {
        assert_eq!(
            recovered.bytes,
            large_payload(),
            "no torn or partial BLOB is acceptable"
        );
        assert_eq!(recovered.summary.byte_size, LARGE_BYTES as u64);
        assert_eq!(recovered.summary.revision, before + 1);
        assert_eq!(store.revision().unwrap(), before + 1);
        assert!(
            store
                .query(query("searchablemarker"))
                .unwrap()
                .entries
                .is_empty()
        );
        assert_eq!(store.query(query("")).unwrap().total, 2);
        assert_eq!(
            store
                .get(&fixture.sibling.id, Some(fixture.sibling.revision))
                .unwrap()
                .bytes,
            OTHER
        );
        if acknowledged {
            assert_eq!(number(root, "large-ack"), before + 1);
        }
    }
    store.shutdown().unwrap();
}

#[test]
fn competing_process_is_refused_without_disturbing_owner() {
    let fixture = Fixture::new();
    let store = HistoryStore::open(&fixture.path()).unwrap();
    FaultChild::spawn("competing-owner", fixture.dir.path()).abrupt_exit();
    fixture.assert_old(&store, fixture.revision);
    store.shutdown().unwrap();
    let reopened = HistoryStore::open(&fixture.path()).unwrap();
    fixture.assert_old(&reopened, fixture.revision);
    reopened.shutdown().unwrap();
}

#[test]
fn corrupt_database_is_refused_without_deleting_or_replacing_evidence() {
    let fixture = Fixture::new();
    let path = fixture.path();
    let mut bytes = fs::read(&path).unwrap();
    assert!(bytes.len() >= 4096);
    // Corrupt an actual previously valid database, rather than merely supplying
    // a foreign file. Leave all remaining pages available for forensic recovery.
    bytes[..16].copy_from_slice(b"CORRUPT HISTORY!");
    fs::write(&path, &bytes).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    let sidecars = ["history.db-wal", "history.db-shm"].map(|name| {
        let path = fixture.dir.path().join(name);
        let contents = fs::read(&path).ok();
        (path, contents)
    });
    for _ in 0..2 {
        assert!(HistoryStore::open(&path).is_err());
        assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
        assert_eq!(fs::read(&path).unwrap(), bytes);
        for (sidecar, contents) in &sidecars {
            if let Some(contents) = contents {
                assert_eq!(&fs::read(sidecar).unwrap(), contents);
            }
        }
    }
}

#[test]
fn file_size_write_failure_is_not_acknowledged_and_preserves_last_commit() {
    let fixture = Fixture::new();
    FaultChild::spawn("file-size-limit", fixture.dir.path()).abrupt_exit();
    let root = fixture.dir.path();
    assert!(
        root.join("efbig-observed").exists(),
        "must reach a real kernel EFBIG"
    );
    assert!(
        root.join("write-rejected").exists(),
        "storage API must report failure"
    );
    let store = HistoryStore::open(&fixture.path()).unwrap();
    fixture.assert_old(&store, number(root, "ready-revision"));
    store.shutdown().unwrap();
}

/// Invoked as the sole test in a subprocess. A normal suite run does nothing;
/// fault scenarios above invoke it explicitly and assert their exit/IO outcome.
#[test]
fn fault_subprocess() {
    let Ok(mode) = std::env::var(CHILD_MODE) else {
        return;
    };
    let root = PathBuf::from(std::env::var_os(CHILD_ROOT).unwrap());
    let path = root.join("history.db");
    match mode.as_str() {
        "initial-schema" => {
            let store = HistoryStore::open(&path).unwrap();
            assert_eq!(store.revision().unwrap(), 0);
            std::process::exit(ABRUPT_EXIT); // No HistoryStore/Turso destructors.
        }
        "committed" => {
            let store = HistoryStore::open(&path).unwrap();
            enable(&store);
            let entry = store.insert("text/plain", OLD).unwrap();
            marker(&root, "committed-id", &entry.id);
            marker(&root, "committed-revision", entry.revision);
            std::process::exit(ABRUPT_EXIT);
        }
        "uncommitted" => block_on(async {
            let database = turso::Builder::new_local(path.to_str().unwrap())
                .with_io(turso::IoBackend::Syscall)
                .experimental_multiprocess_wal(true)
                .experimental_index_method(true)
                .build()
                .await
                .unwrap();
            let conn = database.connect().unwrap();
            conn.execute("PRAGMA foreign_keys=ON", ()).await.unwrap();
            conn.execute("PRAGMA synchronous=FULL", ()).await.unwrap();
            conn.execute("BEGIN IMMEDIATE", ()).await.unwrap();
            let id = fs::read_to_string(root.join("entry-id")).unwrap();
            let sibling = fs::read_to_string(root.join("sibling-id")).unwrap();
            conn.execute(
                "UPDATE payloads SET bytes=?1 WHERE entry_id=?2",
                turso::params![large_payload(), id.clone()],
            )
            .await
            .unwrap();
            conn.execute(
                "UPDATE entries SET byte_size=?1,revision=revision+100 WHERE id=?2",
                turso::params![LARGE_BYTES as i64, id.clone()],
            )
            .await
            .unwrap();
            conn.execute("UPDATE search_content SET content='uncommittedmarker',indexed_content='uncommittedmarker' WHERE entry_id=?1", [id]).await.unwrap();
            conn.execute("DELETE FROM entries WHERE id=?1", [sibling])
                .await
                .unwrap();
            conn.execute("UPDATE store_meta SET revision=revision+100 WHERE id=1", ())
                .await
                .unwrap();
            let mut rows = conn
                .query("SELECT count(*) FROM entries", ())
                .await
                .unwrap();
            assert_eq!(
                rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap(),
                1
            );
            marker(
                &root,
                "uncommitted-executed",
                "all statements executed, no COMMIT",
            );
            std::process::exit(ABRUPT_EXIT); // No implicit rollback on Drop.
        }),
        "large-write" | "file-size-limit" => {
            let store = HistoryStore::open(&path).unwrap();
            enable(&store);
            let id = fs::read_to_string(root.join("entry-id")).unwrap();
            let revision = store.get(&id, None).unwrap().summary.revision;
            let bytes = large_payload();
            marker(&root, "ready-revision", store.revision().unwrap());
            if mode == "large-write" {
                let deadline = Instant::now() + TIMEOUT;
                while !root.join("go").exists() {
                    assert!(Instant::now() < deadline);
                    thread::sleep(Duration::from_millis(1));
                }
                let entry = store
                    .replace(&id, revision, "application/octet-stream", &bytes)
                    .unwrap();
                marker(&root, "large-ack", entry.revision);
                // Keep the process alive if the supervisor hits the post-commit
                // race; the parent then requires the acknowledged new state.
                loop {
                    thread::park_timeout(TIMEOUT);
                }
            } else {
                file_size_failure(&store, &root, &id, revision, &bytes);
                std::process::exit(ABRUPT_EXIT);
            }
        }
        "competing-owner" => {
            assert!(
                HistoryStore::open(&path).is_err(),
                "second process acquired an owned database"
            );
            std::process::exit(ABRUPT_EXIT);
        }
        _ => panic!("unknown fault mode {mode}"),
    }
}

fn file_size_failure(store: &HistoryStore, root: &Path, id: &str, revision: u64, bytes: &[u8]) {
    use rustix::process::{Resource, Rlimit, Signal, getrlimit, setrlimit};
    // Tokio's existing safe signal API installs a handler instead of SIGXFSZ's
    // default termination. This lets the actual write return EFBIG to Turso.
    // The handler and resource limit exist ONLY in this isolated subprocess.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let _signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(
        Signal::XFSZ.as_raw(),
    ))
    .unwrap();
    let limit =
        file_len(&root.join("history.db")).max(file_len(&root.join("history.db-wal"))) + 128 * 1024;
    assert!(
        limit < bytes.len() as u64 / 2,
        "limit must make this BLOB commit impossible"
    );
    let original = getrlimit(Resource::Fsize);
    setrlimit(
        Resource::Fsize,
        Rlimit {
            current: Some(limit),
            maximum: original.maximum,
        },
    )
    .unwrap();
    let mut probe = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(root.join("efbig-probe"))
        .unwrap();
    probe.seek(SeekFrom::Start(limit)).unwrap();
    let error = probe.write_all(b"x").unwrap_err();
    assert_eq!(
        error.raw_os_error(),
        Some(rustix::io::Errno::FBIG.raw_os_error())
    );
    marker(root, "efbig-observed", error);
    let result = store.replace(id, revision, "application/octet-stream", bytes);
    let error =
        result.expect_err("a transaction exceeding the kernel file limit must not be acknowledged");
    marker(root, "write-rejected", format!("{error:?}"));
    // No shutdown/checkpoint/destructors: recovery must handle the failed WAL.
}
