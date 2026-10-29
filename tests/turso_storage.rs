use clip_daemon::{
    backend::{BackendErrorKind, EntryTarget},
    history_store::{ArtifactRecord, HistoryStore, MAX_ENTRY_BYTES, StorePolicy},
    model::HistoryQuery,
};
use tempfile::TempDir;

fn store() -> (TempDir, HistoryStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    store
        .apply_policy(StorePolicy {
            admission: true,
            ..StorePolicy::default()
        })
        .unwrap();
    (dir, store)
}
fn query(text: &str, offset: usize, limit: usize) -> HistoryQuery {
    HistoryQuery {
        query: text.into(),
        offset,
        limit,
        generation: 19,
        collapse_self_echoes: false,
    }
}
fn target(id: &str, revision: u64) -> EntryTarget {
    EntryTarget {
        opaque_id: id.into(),
        expected_revision: revision,
    }
}

#[test]
fn admission_is_closed_initially_and_on_restart_but_edits_work_paused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let s = HistoryStore::open(&path).unwrap();
    assert!(!s.policy().unwrap().admission);
    assert!(s.insert("text/plain", b"secret").is_err());
    s.apply_policy(StorePolicy {
        admission: true,
        ..StorePolicy::default()
    })
    .unwrap();
    let entry = s.insert("text/plain", b"persist").unwrap();
    s.apply_policy(StorePolicy::default()).unwrap();
    let entry = s
        .replace(
            &entry.id,
            entry.revision,
            "text/plain",
            b"edited while paused",
        )
        .unwrap();
    let revision = s.revision().unwrap();
    s.shutdown().unwrap();
    assert!(s.query(query("", 0, 10)).is_err());
    let s = HistoryStore::open(&path).unwrap();
    assert!(!s.policy().unwrap().admission);
    assert_eq!(s.revision().unwrap(), revision);
    assert_eq!(
        s.get(&entry.id, Some(entry.revision)).unwrap().bytes,
        b"edited while paused"
    );
    s.shutdown().unwrap();
}

#[test]
fn exact_bytes_dedup_promotion_and_mime_identity() {
    let (_dir, s) = store();
    let bytes = b"\0\xff\r\n";
    let first = s.insert("application/octet-stream", bytes).unwrap();
    let middle = s.insert("text/plain", b"middle").unwrap();
    let promoted = s.insert("application/octet-stream", bytes).unwrap();
    assert_eq!(first.id, promoted.id);
    assert!(promoted.revision > middle.revision);
    assert_eq!(s.get(&first.id, None).unwrap().bytes, bytes);
    assert_eq!(
        s.get(&first.id, Some(first.revision)).unwrap_err().kind,
        BackendErrorKind::Stale
    );
    assert_eq!(s.query(query("", 0, 100)).unwrap().entries.len(), 2);
    let different = s.insert("application/custom", bytes).unwrap();
    assert_ne!(different.id, first.id);
    assert_eq!(
        s.query(query("", 0, 100)).unwrap().current.unwrap().id,
        different.id
    );
}

#[test]
fn unicode_literal_search_pagination_and_search_replacement_are_sql_consistent() {
    let (_dir, s) = store();
    let a = s
        .insert(
            "text/plain",
            "Straße ΑΒΓ İSTANBUL %_* ' OR 1=1 --".as_bytes(),
        )
        .unwrap();
    s.insert("text/plain", b"other").unwrap();
    for needle in ["straße", "αβγ", "i\u{307}stanbul", "%_*", "' OR 1=1 --"] {
        let page = s.query(query(needle, 0, 10)).unwrap();
        assert_eq!(page.entries.len(), 1, "{needle}");
        assert_eq!(page.entries[0].id, a.id);
        assert_eq!(page.generation, 19);
    }
    assert!(s.query(query("STRASSE", 0, 10)).unwrap().entries.is_empty());
    let page = s.query(query("", 0, 1)).unwrap();
    assert!(page.has_more);
    assert_eq!(page.next_offset, Some(1));
    assert_eq!(s.query(query("", 1, 1)).unwrap().entries[0].id, a.id);
    let zero = s.query(query("", 0, 0)).unwrap();
    assert!(zero.entries.is_empty());
    assert_eq!(zero.next_offset, None);
    assert!(
        s.query(query("", usize::MAX, 100))
            .unwrap()
            .entries
            .is_empty()
    );
    let a = s
        .replace(&a.id, a.revision, "text/plain", b"replacement")
        .unwrap();
    assert!(s.query(query("straße", 0, 10)).unwrap().entries.is_empty());
    assert_eq!(
        s.query(query("replacement", 0, 10)).unwrap().entries[0].id,
        a.id
    );
    // Replacement preserves position, not a fresh capture/promotion.
    assert_eq!(s.query(query("", 1, 1)).unwrap().entries[0].id, a.id);
}

#[test]
fn stale_bulk_delete_is_atomic_and_duplicates_are_idempotent() {
    let (_dir, s) = store();
    let a = s.insert("text/plain", b"a").unwrap();
    let b = s.insert("text/plain", b"b").unwrap();
    let revision = s.revision().unwrap();
    assert_eq!(
        s.remove_many(&[target(&a.id, a.revision), target(&b.id, b.revision + 1)])
            .unwrap_err()
            .kind,
        BackendErrorKind::Stale
    );
    assert_eq!(s.revision().unwrap(), revision);
    assert!(s.get(&a.id, None).is_ok());
    s.remove_many(&[target(&a.id, a.revision), target(&a.id, a.revision)])
        .unwrap();
    assert_eq!(s.revision().unwrap(), revision + 1);
    assert_eq!(
        s.get(&a.id, None).unwrap_err().kind,
        BackendErrorKind::NotFound
    );
}

#[test]
fn favorites_revisions_and_retention_limits_are_transactional() {
    let (_dir, s) = store();
    s.apply_policy(StorePolicy {
        admission: true,
        max_entries: 2,
        max_favorites: 1,
        max_entry_bytes: 8,
        max_total_bytes: 12,
    })
    .unwrap();
    let a = s.insert("text/plain", b"aaaa").unwrap();
    s.favorite(&a.id, a.revision, true).unwrap();
    let favorite = s.get(&a.id, None).unwrap().summary;
    assert!(favorite.favorite);
    assert!(favorite.revision > a.revision);
    assert_eq!(
        s.favorite(&a.id, a.revision, false).unwrap_err().kind,
        BackendErrorKind::Stale
    );
    let b = s.insert("text/plain", b"bbbb").unwrap();
    let c = s.insert("text/plain", b"cccc").unwrap();
    let d = s.insert("text/plain", b"dddd").unwrap();
    assert!(s.get(&b.id, None).is_err());
    assert!(s.get(&a.id, None).is_ok());
    assert!(s.get(&c.id, None).is_ok());
    assert!(s.get(&d.id, None).is_ok());
    let revision = s.revision().unwrap();
    assert!(
        s.replace(&a.id, favorite.revision, "text/plain", b"123456789")
            .is_err()
    );
    assert_eq!(s.revision().unwrap(), revision);
    assert_eq!(s.get(&a.id, None).unwrap().bytes, b"aaaa");
    s.apply_policy(StorePolicy {
        admission: false,
        max_entries: 0,
        max_favorites: 0,
        max_entry_bytes: 1,
        max_total_bytes: 1,
    })
    .unwrap();
    assert!(s.query(query("", 0, 100)).unwrap().entries.is_empty());
}

#[test]
fn replacement_byte_pressure_keeps_target_at_original_position() {
    let (_dir, s) = store();
    s.apply_policy(StorePolicy {
        admission: true,
        max_total_bytes: 10,
        ..StorePolicy::default()
    })
    .unwrap();
    let a = s.insert("text/plain", b"a").unwrap();
    let b = s.insert("text/plain", b"bbbb").unwrap();
    let c = s.insert("text/plain", b"cccc").unwrap();
    let a = s
        .replace(&a.id, a.revision, "text/plain", b"aaaaaa")
        .unwrap();
    assert_eq!(s.get(&a.id, None).unwrap().bytes, b"aaaaaa");
    assert!(s.get(&b.id, None).is_err());
    assert!(s.get(&c.id, None).is_ok());
    assert_eq!(s.query(query("", 0, 10)).unwrap().entries[1].id, a.id);
}

#[test]
fn owner_lock_private_modes_and_symlinks_fail_closed() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("private");
    let path = root.join("history.db");
    let s = HistoryStore::open(&path).unwrap();
    assert!(HistoryStore::open(&path).is_err());
    assert_eq!(
        std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    s.shutdown().unwrap();
    let s = HistoryStore::open(&path).unwrap();
    s.shutdown().unwrap();
    let other = root.join("other.db");
    symlink(&path, &other).unwrap();
    assert!(HistoryStore::open(&other).is_err());
    let evil = root.join("evil.db");
    symlink(&path, root.join("evil.db.lock")).unwrap();
    assert!(HistoryStore::open(&evil).is_err());
    assert!(!evil.exists());
}

#[test]
fn corrupt_empty_and_wrong_identity_files_are_never_reinitialized() {
    for bytes in [b"not a database".as_slice(), b"".as_slice()] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.db");
        std::fs::write(&path, bytes).unwrap();
        assert!(HistoryStore::open(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("foreign.db");
    futures_lite::future::block_on(async {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .with_io(turso::IoBackend::Syscall)
            .build()
            .await
            .unwrap();
        let c = db.connect().unwrap();
        c.execute("CREATE TABLE foreign_data(value TEXT)", ())
            .await
            .unwrap();
        c.execute("INSERT INTO foreign_data VALUES('preserve')", ())
            .await
            .unwrap();
    });
    assert!(HistoryStore::open(&path).is_err());
    futures_lite::future::block_on(async {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .with_io(turso::IoBackend::Syscall)
            .build()
            .await
            .unwrap();
        let c = db.connect().unwrap();
        let mut rows = c.query("SELECT value FROM foreign_data", ()).await.unwrap();
        assert_eq!(
            rows.next()
                .await
                .unwrap()
                .unwrap()
                .get::<String>(0)
                .unwrap(),
            "preserve"
        );
    });
}

#[test]
fn concurrent_mutations_are_serialized_and_stale_edits_cannot_both_commit() {
    let (_dir, s) = store();
    let entry = s.insert("text/plain", b"initial").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let handles = (0..8)
        .map(|n| {
            let s = s.clone();
            let entry = entry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                s.replace(
                    &entry.id,
                    entry.revision,
                    "text/plain",
                    format!("edit {n}").as_bytes(),
                )
                .is_ok()
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        handles
            .into_iter()
            .filter(|h| h.thread().id() != std::thread::current().id())
            .map(|h| usize::from(h.join().unwrap()))
            .sum::<usize>(),
        1
    );
    let handles = (0..8)
        .map(|n| {
            let s = s.clone();
            std::thread::spawn(move || {
                for m in 0..10 {
                    s.insert("text/plain", format!("{n}/{m}").as_bytes())
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(s.query(query("", 0, 200)).unwrap().entries.len(), 81);
}

#[test]
fn artifact_ownership_references_and_cleanup_survive_restart_and_wipe() {
    let (dir, s) = store();
    let path = dir.path().join("history.db");
    let source = s.insert("image/png", b"image").unwrap();
    let artifact = ArtifactRecord {
        path: "/tmp/owned-image.png".into(),
        source_entry_id: source.id.clone(),
        image_identity: "image-digest".into(),
        created_at: 10,
        device: 123,
        inode: 456,
        owner: 1000,
    };
    s.artifact_register(artifact.clone()).unwrap();
    let uri = s
        .insert("text/uri-list", b"file:///tmp/owned-image.png\r\n")
        .unwrap();
    assert!(s.artifact_candidates(100, 128).unwrap().is_empty());
    s.remove_many(&[target(&source.id, source.revision)])
        .unwrap();
    assert!(s.artifact_candidates(100, 128).unwrap().is_empty());
    s.shutdown().unwrap();
    let s = HistoryStore::open(&path).unwrap();
    assert_eq!(
        s.artifact_get(&artifact.path).unwrap(),
        Some(artifact.clone())
    );
    s.remove_many(&[target(&uri.id, uri.revision)]).unwrap();
    assert_eq!(
        s.artifact_candidates(100, 128).unwrap(),
        vec![artifact.clone()]
    );
    s.wipe().unwrap();
    assert!(s.artifact_get(&artifact.path).unwrap().is_some());
    s.artifact_forget(&artifact.path).unwrap();
    assert!(s.artifact_candidates(100, 128).unwrap().is_empty());
}

#[test]
fn validation_and_query_limits_do_not_mutate_revision() {
    let (_dir, s) = store();
    let before = s.revision().unwrap();
    assert!(
        s.apply_policy(StorePolicy {
            max_entry_bytes: MAX_ENTRY_BYTES + 1,
            ..StorePolicy::default()
        })
        .is_err()
    );
    assert!(
        s.apply_policy(StorePolicy {
            max_total_bytes: 0,
            ..StorePolicy::default()
        })
        .is_err()
    );
    assert!(s.insert(&"a".repeat(256), b"x").is_err());
    assert!(s.query(query(&"x".repeat(4097), 0, 10)).is_err());
    assert_eq!(before, s.revision().unwrap());
    for n in 0..205 {
        s.insert("text/plain", n.to_string().as_bytes()).unwrap();
    }
    let page = s.query(query("", 0, usize::MAX)).unwrap();
    assert_eq!(page.entries.len(), 200);
    assert!(page.has_more);
}

#[test]
fn multi_megabyte_payload_is_exact_and_searchable_beyond_preview() {
    let (_dir, s) = store();
    let mut bytes = vec![b'x'; 2 * 1024 * 1024];
    bytes.extend_from_slice(b" unique suffix");
    let entry = s.insert("text/plain", &bytes).unwrap();
    assert_eq!(s.get(&entry.id, None).unwrap().bytes, bytes);
    assert_eq!(
        s.query(query("unique suffix", 0, 10)).unwrap().entries[0].id,
        entry.id
    );
}

#[test]
fn committed_wal_process_helper() {
    let Some(path) = std::env::var_os("CLIP_STORAGE_CRASH_HELPER_PATH") else {
        return;
    };
    let store = HistoryStore::open(std::path::Path::new(&path)).unwrap();
    store
        .apply_policy(StorePolicy {
            admission: true,
            ..StorePolicy::default()
        })
        .unwrap();
    store
        .insert("text/plain", b"committed before abrupt process exit")
        .unwrap();
    std::process::exit(0); // deliberately bypass Rust destructors and shutdown checkpoint
}

#[test]
fn committed_history_recovers_after_abrupt_process_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("history.db");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "committed_wal_process_helper"])
        .env("CLIP_STORAGE_CRASH_HELPER_PATH", &path)
        .status()
        .unwrap();
    assert!(status.success());
    let store = HistoryStore::open(&path).unwrap();
    assert!(!store.policy().unwrap().admission);
    let page = store.query(query("abrupt", 0, 10)).unwrap();
    assert_eq!(page.total, 1);
    assert_eq!(
        store.get(&page.entries[0].id, None).unwrap().bytes,
        b"committed before abrupt process exit"
    );
    store.shutdown().unwrap();
}

#[test]
fn same_version_tampered_schema_is_rejected_without_deleting_data() {
    let (dir, store) = store();
    let path = dir.path().join("history.db");
    store.insert("text/plain", b"preserve").unwrap();
    store.shutdown().unwrap();
    futures_lite::future::block_on(async {
        let db = turso::Builder::new_local(path.to_str().unwrap())
            .with_io(turso::IoBackend::Syscall)
            .experimental_index_method(true)
            .build()
            .await
            .unwrap();
        db.connect()
            .unwrap()
            .execute("DROP INDEX entries_order", ())
            .await
            .unwrap();
    });
    assert!(HistoryStore::open(&path).is_err());
    assert!(path.exists());
}

#[test]
fn literal_search_uses_native_trigram_candidates_without_token_semantic_changes() {
    let (_dir, s) = store();
    let a = s
        .insert(
            "text/plain",
            "preNEEDLEpost café Straße Αλφα ΙΣΤΟΡΙΑ 汉字搜索 punctuation !@#".as_bytes(),
        )
        .unwrap();
    s.insert("text/plain", b"need les unrelated").unwrap();
    for text in ["needle", "eed", "café", "straße", "αλφα", "汉字搜", "!@#"] {
        let page = s.query(query(text, 0, 10)).unwrap();
        assert_eq!(page.total, 1, "{text}");
        assert_eq!(page.entries[0].id, a.id);
    }
    // Repeated grams / query language operators remain literal.
    assert_eq!(
        s.query(query("needle OR unrelated", 0, 10)).unwrap().total,
        0
    );
    s.replace(&a.id, a.revision, "text/plain", b"gone").unwrap();
    assert_eq!(s.query(query("needle", 0, 10)).unwrap().total, 0);
}

#[test]
fn uri_echo_collapse_requires_owned_matching_file_and_unhides_on_source_edit() {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::MetadataExt;
    let (dir, s) = store();
    let path = dir.path().join("generated.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
        .save(&path)
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let source = s.insert("image/png", &bytes).unwrap();
    let mut hash = Sha256::new();
    hash.update(b"clip-daemon:resolved-image:v1:");
    hash.update(b"image/png");
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(&bytes);
    let meta = path.metadata().unwrap();
    s.register_artifact(ArtifactRecord {
        path: path.clone(),
        source_entry_id: source.id.clone(),
        image_identity: hex::encode(hash.finalize()),
        created_at: 0,
        device: meta.dev(),
        inode: meta.ino(),
        owner: meta.uid(),
    })
    .unwrap();
    let uri = url::Url::from_file_path(&path).unwrap().to_string();
    let echo = s.insert("text/uri-list", uri.as_bytes()).unwrap();
    assert_eq!(echo.kind, clip_daemon::model::EntryKind::Image);
    let mut collapsed = query("", 0, 10);
    collapsed.collapse_self_echoes = true;
    assert_eq!(s.query(collapsed.clone()).unwrap().total, 1);
    assert_eq!(s.query(query("", 0, 10)).unwrap().total, 2);
    s.replace(&source.id, source.revision, "text/plain", b"different")
        .unwrap();
    assert_eq!(s.query(collapsed).unwrap().total, 2);
}
