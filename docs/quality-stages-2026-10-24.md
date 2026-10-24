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
