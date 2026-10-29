use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn reservations_bound_bytes_before_payload_copy_and_release_on_drop() {
    let dir = tempfile::tempdir().unwrap();
    let s = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    let first = s.reserve(QUEUE_BYTE_BUDGET / 2).unwrap();
    let second = s.reserve(QUEUE_BYTE_BUDGET / 2).unwrap();
    assert!(s.reserve(1).is_err());
    drop(first);
    let third = s.reserve(QUEUE_BYTE_BUDGET / 2).unwrap();
    drop((second, third));
    assert_eq!(s.inner.submission.lock().unwrap().bytes, 0);
    assert!(s.reserve(QUEUE_BYTE_BUDGET + 1).is_err());
}

#[test]
fn queue_capacity_is_enforced_and_shutdown_drains_all_accepted_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let s = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    let (started, ready) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let worker_store = s.clone();
    let blocked = thread::spawn(move || {
        worker_store.call(move |_| {
            started.send(()).unwrap();
            wait.recv().unwrap();
            Ok(())
        })
    });
    ready.recv().unwrap();
    let completed = Arc::new(AtomicUsize::new(0));
    {
        let submission = s.inner.submission.lock().unwrap();
        for _ in 0..QUEUE_CAPACITY {
            let completed = completed.clone();
            assert!(
                submission
                    .sender
                    .as_ref()
                    .unwrap()
                    .try_send(Box::new(move |_| {
                        completed.fetch_add(1, Ordering::SeqCst);
                    }))
                    .is_ok()
            );
        }
    }
    assert!(s.revision().is_err());
    assert_eq!(s.inner.submission.lock().unwrap().bytes, 0);
    let shutdown_store = s.clone();
    let shutdown = thread::spawn(move || shutdown_store.shutdown());
    release.send(()).unwrap();
    blocked.join().unwrap().unwrap();
    shutdown.join().unwrap().unwrap();
    assert_eq!(completed.load(Ordering::SeqCst), QUEUE_CAPACITY);
    assert!(s.revision().is_err());
    s.shutdown().unwrap();
    HistoryStore::open(&dir.path().join("history.db"))
        .unwrap()
        .shutdown()
        .unwrap();
}

#[test]
fn fenced_worker_shutdown_failure_is_latched_for_every_clone() {
    let dir = tempfile::tempdir().unwrap();
    let s = HistoryStore::open(&dir.path().join("history.db")).unwrap();
    s.call(|db| {
        db.blocked = true;
        Ok(())
    })
    .unwrap();
    assert!(s.revision().is_err());
    let first = s.shutdown().unwrap_err();
    let next = s.clone().shutdown().unwrap_err();
    assert_eq!(first.kind, next.kind);
    assert_eq!(first.to_string(), next.to_string());
}
