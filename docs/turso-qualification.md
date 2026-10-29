# Turso replacement qualification — 2026-10-29

All data and desktops used for qualification were disposable. No production
service activation, old-history read/import/deletion or cutover was performed.
The implementation is forward-only; new committed data remains durable.

## Automated gates

- Release Rust suite: **109 passed**, including both normally ignored private
  large-frame transport tests; no ignored tests in this default-feature run.
  Command: `cargo test --locked --release --all-targets -- --include-ignored --test-threads=1`.
- Real storage tests cover exact 64 MiB payloads, one-byte-over rejection,
  private paths/owner locks, schema tampering/corruption, stale revision races,
  atomic bulk deletion, SQL retention, generated artifact references, indexed
  Unicode/literal search, index rollback and persisted admission closing.
- Fault subprocesses cover acknowledged/initial-schema WAL recovery without
  destructors, uncommitted transaction exit, SIGKILL during a large WAL write,
  competing processes, corruption preservation and actual RLIMIT_FSIZE/EFBIG
  write failure. This is not physical ENOSPC or power-loss simulation.
- Owner-worker tests prove queue/byte reservations, accepted-work draining,
  latched shutdown failure, ambiguous COMMIT/failed ROLLBACK fencing and real
  SQL constraint/index behavior. Native query plans report `QUERY INDEX METHOD fts`.
- Strict all-target/all-feature Clippy and unused-dependency checks pass.
  Formatting and diff checks pass.
- Latest engine contract rerun on the patched release graph passes SQL
  constraints, prepared RETURNING writes, native FTS rollback, large BLOBs,
  crash recovery, verified checkpoint, integrity check and VACUUM.

The separately gated real-Quickshell socket-provider test also passes (one test,
normal and large-frame cases): real shared transport, invalidation, renewal,
owner/peer isolation, disconnect and 16 MiB escaped input. Its QML caller now uses
`cursor_mode`; test padding is removed by the Rust adapter before strict DTO
validation. Five offscreen QML clipboard suites pass **37 QtTest cases**, covering
preview/pagination/selection, pins, process crash and recovery without replay.

## Real isolated desktop

`examples/turso_acceptance.rs` is Rust; its paste target is
`test_support/paste-target.qml`. No Python, shell-script or Lua test helper was
introduced. The runner uses a private HOME/XDG/bus and nested Hyprland, verifies
the unique daemon owner, and retains child identities until process-group cleanup.
Only the nested compositor may connect to the explicitly selected parent display.
All clipboard operations and keystrokes target the nested desktop.

Repeated successful release runs exercise:

- Empty store and missing-intent paused startup; explicit resume/direct policy.
- External capture, SQL search, favorite, revision-stable edit, exact copy,
  current-content no-op recapture and echo exclusion.
- Actual QML text target and a decoy window: hidden-barrier paste targets the
  original window, transfers exact text, and cannot replay or type into the decoy.
- Actual Satty annotation save, escape and API cancellation, including terminal
  operation receipts, child exit and clipboard/history integrity. Saving the
  unmodified image proves the native completion path, not drawing-tool geometry.
- Actual grim region screenshot, HiDPI-aware pixel dimensions and exact stored
  PNG restoration.
- Rust CLI and JSONL client, graceful restart persistence and saved private startup.
- Pause/private barriers, skipped resume bootstrap, sensitive offer exclusion and
  sensitive publication, file cut/URI MIME aliases, PNG bytes and primary exclusion.
- Exact configured capture byte boundary and one-byte-over rejection.
- Durable delete, compositor loss/reconnect, wipe/editor fencing, no replay and
  preservation of arbitrary user files referenced by URIs.

Three consecutive worker rehearsals passed. Final managed rehearsal:
`/tmp/clip-at-8Ceihc`; actual installed-package activation:
`/tmp/clip-at-HoCNBO` and final output `/tmp/clip-at-uXIv9l`. Logs contain synthetic data only. Intermediate failures
exposed parent-output focus races and a logical/physical screenshot-size mismatch;
the final harness disables parent-backed outputs inside its private compositor
and accounts for grim's output scale. Mutations/shortcuts are never retried. Capture-to-visible samples
were 42–50 ms with approximately 40 ms polling resolution, not a precise latency
percentile. Pure headless Hyprland on this machine lacks a working backend;
`--nested` is explicit and never silently falls back to the normal clipboard.
`local-build build . --no-link --print-out-paths` successfully builds and runs
all 109 default-feature tests in the Nix sandbox. Installed output
`/nix/store/blb28zpcs259cxg7m471zi7qn2sykjzh-clip-daemon-0.1.0` passes private
D-Bus activation and the full applicable desktop rehearsal; its runtime closure
contains no Ringboard package. Restart persistence is separately covered by
managed `--binary` mode. Packaging qualification caught and fixed Crane stubbing
the patched path crate, a missing source-contract input, and a test that previously
read host preferences.

## Performance and capacity

Synthetic release benchmark uses the production owner/store, FULL commits,
1 KiB text payloads, real retention, SQL counts/pages and selective literal search.
The large run retained 10,000 entries after 11,000 captures. Timing is not an
idle-machine microbenchmark: other build work overlapped this run.

| Large-history operation | p50 | p95 | p99 |
| --- | ---: | ---: | ---: |
| Capture with retention | 31.03 ms | 83.90 ms | 125.69 ms |
| First page (40 rows) | 0.75 ms | 1.10 ms | 1.24 ms |
| Deep page | 3.64 ms | 5.41 ms | 6.26 ms |
| Selective full-text search | 1.84 ms | 3.14 ms | 3.53 ms |

The final default-capacity run retained 1,000 entries after 2,000 captures in
27.0 s. p95 capture/first/deep/search: **18.61 / 0.30 / 0.88 / 1.82 ms**;
p99: 21.37 / 0.36 / 0.91 / 2.05 ms. Shutdown was 17 ms, peak RSS 529,528 KiB.

For the 10,000-entry run, 16/64 MiB exact commit-and-read: 210/797 ms. Peak process RSS was 530,572 KiB;
post-workload RSS 110,704 KiB. Shutdown/checkpoint took 69 ms. Three further
capture/wipe cycles used approximately 133–135 MB including WAL; after checkpoint
132,153,344 bytes remained allocated. Freed pages are reusable; ordinary wipe is
not a promise to shrink or securely erase the database. The separate engine probe
verified explicit VACUUM shrinking 67.3 MB to 32 KiB.

Operational qualification budgets for the measured 10,000-entry workload:
p95 capture ≤150 ms, first/deep page ≤50 ms, selective search ≤100 ms,
64 MiB commit/read ≤2.5 s, peak RSS ≤1 GiB, shutdown ≤5 s. All measured values
meet these budgets. Very short/punctuation searches and >64 KiB unindexed texts
use SQL verification and have different selectivity costs. The 100,000/10,000
count settings ceilings are validation limits, not a measured latency promise.
Default capacity is 1,000 regular/100 favorite entries, 16 MiB per entry and
256 MiB raw payload; the hard entry ceiling is 64 MiB.

The old `history-benchmark.md` measured **warm in-memory projection only** (e.g.
84.7 µs for a 750-row first page), excluding database access, hashing and IPC.
Those results are not comparable to full durable SQL operations, and no speedup
claim is made. The old full-history projection and its benchmark adapter are gone.

## Dependencies and release boundaries

Turso public crate/core/SDK are 0.8.3. Native FTS is enabled explicitly; syscall I/O
is used. A scoped upstream Rust source snapshot of `tantivy-sstable` changes its
compression requirement to released zstd 0.14.3 / zstd-safe 8.2.0, fixing three
audit vulnerability findings that Turso's current dependency range otherwise
retains. Tokio is updated to 1.54.1. See `vendor/README.md`; no advisories are
suppressed. `cargo audit` exits successfully but still reports four upstream
warnings (lru panic safety and three D-Bus crate maintenance notices).

Physical power loss and a real graphical login remain operator/hardware checks,
not simulated proof. Deployment is deliberately separate: stop/mask both retired
Ringboard services, rebuild the matching QML caller and activate only the new
clipboard daemon. Old history/favorites are not imported. See installation.md.
