//! Synthetic production-store benchmark; never opens the user's database.
use anyhow::{Result, ensure};
use clip_daemon::{
    backend::EntryTarget,
    history_store::{HistoryStore, StorePolicy},
    model::HistoryQuery,
};
use std::{path::Path, time::Instant};

fn query(text: &str, offset: usize) -> HistoryQuery {
    HistoryQuery {
        query: text.into(),
        generation: 1,
        offset,
        limit: 40,
        collapse_self_echoes: true,
    }
}
fn percentile(values: &mut [u128], percent: usize) -> u128 {
    values.sort_unstable();
    values[((values.len() - 1) * percent) / 100]
}
fn timed<T>(values: &mut Vec<u128>, op: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = op();
    values.push(start.elapsed().as_micros());
    result
}
fn disk(path: &Path) -> Result<u64> {
    let mut total = 0;
    for file in std::fs::read_dir(path)? {
        total += file?.metadata()?.len();
    }
    Ok(total)
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        args.is_empty() || args == ["--large"],
        "only --large is accepted; no database paths"
    );
    let retained = if args.is_empty() { 1000 } else { 10_000 };
    let captures = retained + 1000;
    let root = tempfile::tempdir()?;
    let path = root.path().join("history/history.db");
    let store = HistoryStore::open(&path)?;
    store.apply_policy(StorePolicy {
        admission: true,
        max_entries: retained as u32,
        ..Default::default()
    })?;
    let mut capture = Vec::new();
    let start = Instant::now();
    for i in 0..captures {
        let text = format!("clipboard unique{i} {}", "word ".repeat(200));
        timed(&mut capture, || store.insert("text/plain", text.as_bytes()))?;
    }
    let ingest = start.elapsed();
    ensure!(store.query(query("", 0))?.total == retained);
    let mut page = Vec::new();
    let mut search = Vec::new();
    let mut deep = Vec::new();
    for _ in 0..100 {
        ensure!(
            timed(&mut page, || store.query(query("", 0)))?
                .entries
                .len()
                == 40
        );
        ensure!(
            timed(&mut deep, || store.query(query("", retained - 40)))?
                .entries
                .len()
                == 40
        );
        ensure!(
            timed(&mut search, || store
                .query(query(&format!("unique{}", captures - 1), 0)))?
            .total
                == 1
        );
    }
    println!(
        "profile={} entries={retained} retained_after={captures} ingest_ms={}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        ingest.as_millis()
    );
    for (name, values) in [
        ("capture_with_eviction", &mut capture),
        ("first_page", &mut page),
        ("deep_page", &mut deep),
        ("selective_search", &mut search),
    ] {
        let p50 = percentile(values, 50);
        let p95 = percentile(values, 95);
        let p99 = percentile(values, 99);
        println!("{name}: p50_us={p50} p95_us={p95} p99_us={p99}");
    }
    let mut policy = store.policy()?;
    policy.max_entry_bytes = 64 * 1024 * 1024;
    store.apply_policy(policy)?;
    for mib in [16, 64] {
        let bytes = vec![0x81; mib * 1024 * 1024];
        let start = Instant::now();
        let entry = store.insert("application/octet-stream", &bytes)?;
        ensure!(store.get(&entry.id, Some(entry.revision))?.bytes == bytes);
        println!(
            "payload_mib={mib} commit_read_ms={}",
            start.elapsed().as_millis()
        );
        store.remove_many(&[EntryTarget {
            opaque_id: entry.id,
            expected_revision: entry.revision,
        }])?;
    }
    for cycle in 0..3 {
        store.wipe()?;
        for i in 0..100 {
            store.insert(
                "application/octet-stream",
                &vec![(i + cycle) as u8; 64 * 1024],
            )?;
        }
        println!(
            "capture_wipe_cycle={cycle} physical_bytes={}",
            disk(path.parent().unwrap())?
        );
    }
    let start = Instant::now();
    store.shutdown()?;
    println!(
        "shutdown_ms={} checkpointed_bytes={}",
        start.elapsed().as_millis(),
        disk(path.parent().unwrap())?
    );
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status
            .lines()
            .filter(|line| line.starts_with("VmHWM:") || line.starts_with("VmRSS:"))
        {
            println!("{line}");
        }
    }
    let reopened = HistoryStore::open(&path)?;
    ensure!(reopened.query(query("", 0))?.total == 100);
    ensure!(!reopened.policy()?.admission);
    reopened.shutdown()?;
    println!(
        "PASS: production-store retention, pagination, search, payload, reuse, shutdown and restart benchmark"
    );
    Ok(())
}
