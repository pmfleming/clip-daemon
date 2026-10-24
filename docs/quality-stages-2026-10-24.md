# Five follow-up quality stages

Starting point: `51bd16b`. Priorities: operation ownership, subprocess lifetime,
history projection, bounded capture proofs, and native capture qualification.
Each stage is validated and committed independently. Logs live in
`target/quality-stages/`; no quality thresholds or exclusions are changed.

## 1. Authoritative operation ownership

Added a backend snapshot of live editing/committing jobs. API dispatch and cancel
reconcile ownership against it, instead of relying on lossy broadcast delivery.
Early completion cannot resurrect ownership; failed/completed jobs disappear on
the next dispatch/cancel. A dormant API retains at most its last live snapshot,
not an ever-growing completion history. This deliberately avoids a background
observer, terminal tombstones, and subscriber-dependent cleanup.

Fixed a separate authorization defect: a missing ID was previously equivalent
to a stored `None` owner, allowing unowned cancellation without registration.
Cancellation no longer holds the ownership mutex across backend work.

One table-driven test uses real Ringboard operation tasks (no database/desktop)
to exercise success/failure before and after registration, missing records,
wrong owners, unowned jobs, and repeated cancellation. The existing commit-drain
test verifies committing jobs remain in the authoritative snapshot.

Validation: all-target/all-feature tests and strict Clippy pass; formatting and
`git diff --check` pass (`stage1.log`).

## 2. Screenshot subprocess lifetime

Direct and interactive grim captures now share the async timeout/status path.
Timeout and wait failures explicitly kill and reap the child; dropped callers
are covered by Tokio's kill-on-drop behavior. Removed the separate blocking
25-ms poll loop. Direct captures validate dimensions at the backend boundary,
use a scoped staging-file guard, and hold the history transaction only during
publication, not while waiting for grim. The guard moves into blocking commit
work so caller cancellation cannot delete a file still being published.

The existing command test now checks successful/nonzero/missing commands,
timeout, and cancellation of a real child, including eventual process reaping
and staged-file deletion. No desktop programs or real clipboard are involved.
All-target/all-feature tests, strict Clippy, formatting, and diff checks pass
(`stage2.log`).

## 3. Immutable history projection metadata

Cache visible-row indexes, current selection, identity bindings, and generated
file references once per history token and collapse policy (at most two views).
Pages share bindings/references using `Arc`; mutations use copy-on-write and
still revalidate full content before authorizing actions. This intentionally
adds a small amount of cache state instead of cloning all IDs on every page.

Two focused tests exercise the real projection: search/full-text sets, blank
and missing matches, zero/overflowing page bounds, echo collapse and missing
sources, current selection outside the page, protected hidden artifacts,
incomplete snapshots, and stale ID/revision rejection without corrupting the
shared metadata. Every page asserts pointer reuse, not just equal contents.

All 12 benchmark semantic fingerprints match the pre-stage results. Warm first
page allocations at 5,000 rows fall **5,323 → 310**, allocated bytes
**1,045,293 → 44,241**; at 20,000 rows, **20,325 → 310** allocations. These are
allocation measurements from the opt-in benchmark in debug mode, not a release
latency claim. Cold construction and storage I/O are outside this measurement.
All-target/all-feature tests, strict Clippy, formatting, and diff checks pass
(`stage3.log`, `stage3-before-benchmark.log`).
