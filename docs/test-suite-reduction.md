# Test-suite reduction

## Current baseline and result

Baseline: **`9daa32f`**, before this test-only change. Inventory command:

```sh
cargo test --all-targets --all-features --locked -- --list
```

| Measure | Before | After |
| --- | ---: | ---: |
| Discovered project Rust tests | 60 | 40 |
| Active tests | 59 | 39 |
| Existing opt-in, ignored history benchmark | 1 | 1 |
| Physical Rust lines under `src/` and `tests/` | 10,948 | 10,590 |

`60 × 0.67 = 40.2`, rounded to **40**: **20 fewer tests (33.33%)**.
The count includes the existing benchmark when its feature is enabled. Vendored
matcher tests and external Python/desktop acceptance checks are outside the
root-package Cargo inventory and were not changed. No tests were newly ignored,
feature-gated, filtered from discovery, or moved to an uncounted harness.
No permanent count ceiling was added.

Production code is identical after removing inline test modules and normalizing
whitespace. The reduction removes fixtures/assertions as well as test functions:
**358 Rust lines removed net**. It does not retain deleted tests as unregistered
helper functions called by a catch-all test.

## Selection decisions

| Area | Before → after | Decision and retained protection |
| --- | ---: | --- |
| Actions | 5 → 3 | Remove duplicate partial-publication/single-use fixture; assert those outcomes in the existing published/unpublished edit scenario. Use one conflict table for stale session versus changed revision, retaining unchanged-content checks. Keep ordinary paste/image publication failures independently. |
| Admission and worker | 5 → 3 | Generation invalidation is checked across the retained in-flight pause/resume scenario. Exercise lost replies and sink panics through the sticky worker-shutdown scenario, checking both gate and controller refusal to retry. Remove the duplicate gate-only setups. |
| Capture transfer | 2 → 1 | Replace separate memfd and pipe fixtures with one nonblocking-pipe policy table. Keep complete binary content, exact/over limits, blank filtering, WouldBlock versus EOF, idle/absolute deadlines, and budget release. |
| Wayland polling | 1 → 0 | Remove private descriptor-layout/zero-allocation assertions. They constrain the implementation, not the public capture contract. No claim of live compositor coverage replaces them. |
| Classification and resolved content | 5 → 4 | Move MIME-authority cases to the daemon's resolved-content boundary rather than testing the classifier helper separately. Keep spoof-resistant bounded previews, image-file interpretation, escaped names, cut semantics, and unsafe-file rejection. |
| Subscription | 2 → 1 | Remove duplicate serialization checks for the framework-owned `Gap` DTO; maximum-u64 string encoding is tested in daemon-core. Keep history outage/recovery suppression behavior. |
| Native search | 2 → 1 | Remove the allocation-count/catalog-copy microtest. Blank/Unicode-whitespace queries now compare complete returned rows against ordinary history through the API; empty history remains covered. Keep cursor authentication, expiry, scope, and tampering tests. |
| Model | 2 → 0 | Drop the label-equals-serde-name restriction, which unnecessarily couples future display wording to the wire format. Drop iterator visitation mechanics: real cached projection tests retain paging, overflow offsets, current-entry flags, bindings outside the page, and incomplete projections. |
| Artifacts | 4 → 2 | Remove standalone scanner-boundary and private file-hash fixtures. The lifecycle scenario checks missing/oversized/symlink echoes through registry matching, referenced and ambiguous history, grace/active selection, failed-delete retries, and unrelated-file preservation. Keep full-content echo identity and manifest reload separately. |
| Capture dedup hint | 1 → 0 | Remove chunk/budget/cursor-position mechanics for a non-authoritative optimization. Keep full stored-content identity and length-change regressions, exact IPC framing, and revision-checked mutation behavior. The real server's dedup path is not executed by these Cargo tests. |
| Selection | 3 → 1 | Remove the exact MIME-alias whitelist microtest. Check MIME/byte admission, metadata-versus-read growth, file I/O failure, rejection before publisher side effects, and file-operation header size in one publication scenario. Drop assertions fixing the exact number of bytes consumed by a failed read. |
| Session | 2 → 1 | Drop the separate address-format whitelist test; retain null/injection rejection with expiry/end protection in the target-safety scenario. |
| Settings | 5 → 4 | Remove scalar snapshot/Copy mechanics. Keep malformed persisted settings, failed persistence, unverified privacy, retry, and abandoned-transition fencing. |
| API publication | 6 → 5 | Remove repetitive publishText fixture/setup; its success, empty-input and unknown-field cases join the existing publication/validation matrix. Keep wipe fencing, deletion atomicity, type-action safety and cursor behavior independently. |
| JSONL executable | 2 → 1 | Send the escaped oversized frame as the first request in the existing 4,000-request churn scenario. One isolated subprocess now checks frame acceptance, rejection-before-dispatch, route preservation, exactly-once replies, and EOF draining together. |

Protocol-registry consistency, operation/subscription correlation, editor literal
argument handling and failed exit, PNG full decoding and limits, screenshot
process timeout/selector cancellation, operation cancellation versus committing
cleanup, and streaming UTF-8/search failure regressions remain dedicated tests.
No test singles out an obsolete method name or requires a speculative future
implementation.

## Validation and coverage tradeoff

- **39 active tests pass**, repeatedly across three complete all-target/all-feature runs.
- The unchanged ignored benchmark was executed separately: all **12** history
  result fingerprints, allocation counts, and allocated-byte totals match the
  preceding reviewed implementation.
- Strict all-target/all-feature Clippy, package formatting, doctests, and
  `git diff --check` pass.
- Fresh Rust Quality Lens correctness inventory, test-quality, hotspot, and
  duplication evidence was collected without changing its configuration.
- Baseline/final LLVM line coverage is **4203/7420 (56.64%) → 3959/7200 (54.99%)**.
  These totals include test code; deleting covered tests changes the denominator.
- On identical production source, RQLens-style **executable segment-start** hits
  are **2252/4596 (49.00%) → 2224/4596 (48.39%)**. This is a narrower signal, not
  LLVM's full line-coverage calculation. The principal losses are private poll
  and dedup-hint routines, label arms, alias selection, and the Gap adapter.
  Coverage preservation is not claimed numerically.

The uncovered optimization/adapter paths are a deliberate tradeoff, not newly
verified behavior. Live Wayland/Ringboard/desktop acceptance was not run, and
unchanged external tests are not counted as executed coverage. RQLens's
macro/cross-file inventory limitations still apply. The sibling framework has
unrelated formatting differences, so formatting was checked with
`cargo fmt -p clip-daemon -- --check` rather than modifying that checkout.

Baseline source/inventory/coverage: `target/test-reduction-v2-before/`.
Final inventories, repeat-run logs, LLVM JSON and RQLens artifacts:
`target/test-reduction-v2-after/`. Summary:
`target/test-reduction-v2-comparison.json`. All evidence is ignored build output.
The previous 72 → 48 reduction is historical and remains in Git history; it is
not this pass's baseline.
