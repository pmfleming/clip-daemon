//! Opt-in, ignored release benchmark; no desktop or real history access.
use std::{hint::black_box, time::Instant};

use super::*;
use crate::model::EntryKind;

fn project(cached: &CachedProjection, query: &HistoryQuery) -> QueryProjection {
    cached.project(query)
}

fn fixture(size: usize) -> CachedProjection {
    CachedProjection {
        current_id: Some(size as u64 - 1),
        complete: true,
        candidates: (0..size)
            .map(|index| QueryCandidate {
                raw_id: index as u64,
                resolved: ResolvedEntry {
                    summary: EntrySummary {
                        id: format!("entry-{index:032x}"),
                        revision: index as u64 + 1,
                        kind: EntryKind::Text,
                        mime: "text/plain;charset=utf-8".into(),
                        byte_size: 256,
                        favorite: index % 10 == 0,
                        current: false,
                        preview: format!(
                            "{} {}",
                            if index % 100 == 0 { "needle" } else { "text" },
                            "x".repeat(256)
                        ),
                    },
                    generated_path: None,
                    echo_source_id: None,
                },
            })
            .collect(),
    }
}

#[test]
#[ignore = "run explicitly in release mode with --features benchmarks"]
fn history_projection() {
    let iterations = 30;
    for size in [750, 5_000, 20_000] {
        let cached = fixture(size);
        for (name, needle, offset) in [
            ("first-page", "", 0),
            ("pagination", "", size / 2),
            ("search", "needle", 0),
            ("no-match", "absent", 0),
        ] {
            let query = HistoryQuery {
                query: needle.into(),
                generation: 1,
                offset,
                limit: 100,
                collapse_self_echoes: true,
            };
            let result = project(&cached, &query);
            let semantic =
                serde_json::to_vec(&(result.entries, result.current, result.matched)).unwrap();
            let fingerprint = hex::encode(Sha256::digest(semantic));
            for _ in 0..5 {
                black_box(project(&cached, &query));
            }
            // Timing and allocation instrumentation are separate runs. Counter
            // overhead is common to both revisions, not a production allocator.
            let mut timings = Vec::with_capacity(iterations);
            for _ in 0..iterations {
                let start = Instant::now();
                black_box(project(&cached, &query));
                timings.push(start.elapsed().as_nanos());
            }
            timings.sort_unstable();
            let allocations = allocation_counter::measure(|| {
                black_box(project(&cached, &query));
            });
            assert_eq!(allocations.bytes_current, 0);
            println!(
                "QUERY_BENCH {}",
                serde_json::json!({
                    "size": size, "scenario": name, "iterations": iterations,
                    "median_ns": timings[iterations / 2], "p95_ns": timings[iterations * 95 / 100], "samples_ns": timings,
                    "allocation_count": allocations.count_total, "allocated_bytes": allocations.bytes_total,
                    "peak_bytes": allocations.bytes_max, "fingerprint": fingerprint,
                })
            );
        }
    }
}
