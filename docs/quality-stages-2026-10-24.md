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
