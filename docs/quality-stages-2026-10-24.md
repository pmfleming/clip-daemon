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

## 4. Bounded capture proofs and real-file comparison

Capture now reuses content inspection with a zero-byte preview, so proof hashing
needs no preview allocation. Both capture and history inspection read at most
advertised size plus one byte and reject growth/truncation before accepting a
digest. Previously capture ignored the copied byte count, while history could
follow an indefinitely growing file. Hash domains and successful digests remain
unchanged; changed capture files now fail closed before candidate lookup/IPC.

Extended the existing identity test with preview/no-preview digest equivalence
and a counting cursor proving the one-extra-byte bound. One capture test uses a
real temporary file and interrupted seven-byte reads to check multi-chunk
matching, byte differences beyond the first chunk, exact/exhausted budgets,
truncation, empty content, and preservation of the admitted FD's seek position.
All-target/all-feature tests, strict Clippy, formatting, and diff checks pass
(`stage4.log`). Live policy-engine IPC remains a separate qualification gap.

## 5. Native capture against an isolated compositor

Separated connection acquisition from the native capture loop and reused a
single Session constructor in the worker and fixture. The fixture exchanges
actual WLR data-control requests and file descriptors over a socket pair; it
never changes environment variables, opens the desktop compositor, or touches
real history. One scenario matrix covers initial import versus resume suppression,
late sensitive markers, PRIMARY exclusion, blank-MIME fallback, oversized data
without fallback, preserved UTF8_STRING requests, full-queue drops, device finish,
connection loss, and gate closure while a sender is stalled. The last case also
checks that the receive FD was closed and no job was admitted.

The operation fixture now propagates channel/launch failures instead of panicking
inside its worker. This also avoids RQLens misclassifying unwraps inside a
`cfg(test)` impl as production panic paths; no lint exclusions were added.

All-target/all-feature tests (**47 passed, 1 benchmark ignored**), strict Clippy,
doctests, cargo-machete, formatting, and diff checks pass (`stage5-final.log`).
The native fixture also passed three consecutive extra runs (`stage5.log`).
The opt-in projection benchmark was explicitly executed in stage 3.

## Final evidence and trade-offs

Used the same pinned local RQLens binary/config as the preceding review, running
`hotspots`, `clones`, `escape-hatches`, `reliability`, `locality`, and `leverage`.
Baseline artifacts are `target/quality-20261024/after/` (source at `51bd16b`);
final artifacts are `target/quality-stages/after/`. Coverage uses locked,
all-feature `cargo llvm-cov --workspace --json` with totals filtered to `src/`.

| Signal | Before | After |
| --- | ---: | ---: |
| Covered / instrumented source lines | 4,113 / 7,358 | 4,947 / 7,785 |
| Source line coverage | 55.90% | 63.55% |
| Native capture loop coverage | 0% | 84.40% |
| Ringboard capture module coverage | 0% | 43.80% |
| Ringboard facade/projection coverage | 0% | 36.06% |
| Function cognitive complexity, sum | 605 | 652 |
| Function cyclomatic complexity, sum | 1,796 | 1,865 |
| Function Halstead effort, sum | 4,351,879 | 4,937,384 |
| Mean observed-reuse leverage | 24.05 | 24.74 |
| Mean locality | 99.29 | 99.21 |
| Physical Rust lines in src/tests | 11,056 | 11,848 |
| Test functions, including benchmark | 43 | 48 |

**These stages do not improve every static metric.** They prioritize lifecycle
correctness, allocation reduction, and execution of previously untested code.
The explicit compositor fixture and new scenario tests add code/complexity;
source totals include inline tests. Projection's individual Halstead effort
falls 46,752 → 30,229, while the shared cache construction is a separate cost.
Warm-page allocation reduction is measured separately from these syntax scores.
Token duplicate groups rise 11 → 14, including protocol fixture boilerplate;
no macros were added to hide it. Escape-hatch and production panic-path findings
remain zero. Architecture scores remain advisory, and Wayland macro expansion
keeps RQLens inventory evidence partial.

Remaining gaps: the ext-data-control protocol branch, live compositor reconnect/
seat churn, and real Ringboard policy-engine/database I/O. Aggregate coverage
includes inline test code; it is not a claim that the entire backend now has
end-to-end qualification. No mutation campaign or release latency claim is made.
