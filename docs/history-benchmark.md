# Cached history projection benchmark

Run with `python3 scripts/benchmark-history.py` after resolving the locked dependencies (`cargo check --offline --features benchmarks` if necessary). The driver archives baseline `fc1ac57` into a temporary directory, injects the same benchmark with a legacy-projection adapter, and builds both revisions with the current manifest/lockfile and Rust 1.95 release profile. No real clipboard data or desktop connection is used.

The `benchmarks` feature enables an ignored unit benchmark so private projection types do not become public API. `allocation-counter` is a dev-only dependency; production uses its ordinary allocator. Timing runs are separate from allocation-counting runs, although the instrumented allocator is linked into both benchmark binaries. Five warmups precede each scenario. Three rounds alternate revision order, giving 90 timing samples per case. All 12 output fingerprints (page summaries, current entry, and total matches) agree between implementations. Allocations return to zero after each projection is dropped.

## 2026-09-08 results

Synthetic histories contain 256-byte previews, 10% favorites, and a 1% search-match rate. Pages contain at most 100 entries. These numbers characterize warm projection plus result destruction—not database reads, hashing, mutex contention, D-Bus, thumbnail cleanup, or end-to-end UI latency.

| Entries | Scenario | Before median µs | After median µs | Speedup | Allocated bytes reduction |
|---:|---|---:|---:|---:|---:|
| 750 | first page | 235.7 | 84.7 | 2.78× | 76.83% |
| 750 | pagination | 233.2 | 85.1 | 2.74× | 76.83% |
| 750 | search | 281.5 | 114.3 | 2.46× | 66.19% |
| 750 | no match | 280.2 | 115.5 | 2.43× | 66.51% |
| 5,000 | first page | 2,212.3 | 607.6 | 3.64× | 80.16% |
| 5,000 | pagination | 2,038.7 | 614.0 | 3.32× | 80.16% |
| 5,000 | search | 2,446.8 | 860.0 | 2.85× | 66.67% |
| 5,000 | no match | 1,897.1 | 849.0 | 2.23× | 66.96% |
| 20,000 | first page | 13,682.6 | 4,038.4 | 3.39× | 80.69% |
| 20,000 | pagination | 13,535.0 | 4,084.3 | 3.31× | 80.69% |
| 20,000 | search | 15,056.5 | 5,041.3 | 2.99× | 66.82% |
| 20,000 | no match | 14,564.6 | 4,911.6 | 2.97× | 66.96% |

At 20,000 entries, first-page allocation count falls from 120,048 to 20,333 and allocated bytes from 23,675,109 to 4,570,589. Search still allocates lowercase preview/MIME strings for all candidates: this is a remaining optimization opportunity, not an allocation-free query claim.

Machine-readable samples, p95, allocation counts, peak bytes, source/toolchain provenance, and logs are retained under ignored `target/history-benchmark/`. An initial single-round run was noisy (including one slightly slower no-match result); the interleaved repeated run above is the reported experiment. Re-run on representative hardware before setting latency budgets. No performance thresholds have been imposed on ordinary tests.
