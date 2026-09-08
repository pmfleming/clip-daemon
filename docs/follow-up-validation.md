# Follow-up validation

## Step 3 — deterministic failure and concurrency coverage

Added tests for:

- failed copy/image-as-file publication never arming a real-target paste session;
- edit partial-publication results retaining the committed content, with single-use leases;
- a revision change during an edit rejecting the stale commit without overwriting new content;
- annotation cancellation both before readiness and while running, with exactly one terminal event and staged-file cleanup;
- expired/ended sessions never triggering paste;
- overlapping queries and eight competing replacements returning consistent snapshots, with only one writer succeeding against revision 1.

The fake backend previously checked replacement and bulk-delete revisions under a separate lock from mutation. It now validates and mutates under one write lock. Publication fault injection and synthetic paste-target helpers are compiled only in unit tests. No tests inject shortcuts into the real desktop.

`cargo test --all-targets --locked`: **39 passed**. The concurrent-query test also passed ten repeat runs. `cargo clippy --all-targets --all-features --locked -- -D warnings` passes. RQLens coverage reports **49.36% lines**, **47.98% functions**, and **48.02% regions** (versus 44.63% lines after the initial refactor).

Limitations: partial-publication API coverage uses an injected backend failure, not a compositor outage. Fake-backend atomicity is not a claim that Ringboard's multi-request IPC protocol offers compare-and-swap transactions against external capture/mutation. The live success/cancellation paths are exercised separately by the step-1 harness.

## Step 4 — remaining hotspot simplification

| Function | Hotspot before → after | Cognitive before → after | Cyclomatic before → after |
|---|---:|---:|---:|
| artifact pruning | 36.62 → 23.30 | 5 → 3 | 8 → 5 |
| action launching | 35.73 → 27.44 | 1 → 1 | 11 → 9 |
| entry inspection | 34.38 → 20.22 | 4 → 1 | 6 → 5 |

Pruning now takes a minimum age rather than a redundant force flag: `clear_all` already clears the active selection and can pass zero age. Failed file removals remain registered. Launch dispatch propagates one result instead of repeating propagation in every arm; file launches pass the original `OsStr` path rather than a lossy UTF-8 conversion. Inspection uses a bounded prefix read followed by streaming `io::copy` into SHA-256, retaining the full-content digest and checked size comparison without a manual chunk loop.

Tests cover forced pruning of young active files and independent full-stream digest equality across preview-boundary sizes and interrupted reads. **40 tests pass**, and strict all-feature Clippy passes. The function scores above come from fresh RQLens measurements immediately before/after this step, not changed thresholds or exclusions.
