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
