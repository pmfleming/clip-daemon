# Test-suite reduction

## Baseline and result

This pass starts from the **working tree after the capture/search quality review**,
not from Git HEAD. Count command:

```sh
cargo test --all-targets --all-features --locked -- --list
```

| Measure | Before | After |
| --- | ---: | ---: |
| Registered project Rust tests | 72 | 48 |
| Runnable tests | 71 | 47 |
| Existing opt-in, ignored benchmark | 1 | 1 |
| Physical Rust lines under `src/` and `tests/` | 10919 | 10533 |

`72 × 0.67 = 48.24`; rounding to whole tests gives **48**, a reduction of
**24 tests (33.33%)**. No tests were newly ignored, feature-gated, or hidden from
discovery. The separate 39 backend/capture/desktop acceptance checks and vendored
matcher tests are outside this Cargo test count and were not removed. No count
ceiling was added: future behavior and regressions should still receive tests.

Production source outside inline test modules is unchanged from this pass's
baseline (apart from a trailing blank line). The reductions remove redundant
fixtures/assertions as well as test functions: **386 Rust lines removed net**.

## Selection decisions

Counts include the unchanged ignored benchmark where applicable.

| Area | Before → after | Removed overlap / retained protection |
| --- | ---: | --- |
| `tests/capture_contract.rs` | 1 → 0 | Retire the pre-migration SDK-only test. `capture::policy` runs the same MIME fixtures through the actual collector policy, plus overflow/sensitive-marker cases. |
| `tests/client.rs` | 2 → 1 | The isolated routed-client churn test already verifies exactly-once EOF draining. Remove the one-request test that could contact the user's session service. |
| `tests/api.rs` | 8 → 6 | Remove concurrency testing of `FakeBackend`'s lock and the standalone fake pagination fixture. Real Ringboard concurrent-mutation checks remain; the bulk-delete scenario checks offset/limit forwarding and final-page metadata. |
| `actions` | 4 → 3 | Replace the private `complete_text` helper test with a truncated-launch rejection through the API action-policy test. Keep failed-publication paste safety and both edit regressions. |
| `capture` admission | 4 → 3 | Error and panic submissions exercise one fail-closed contract table, including rejected resume after uncertainty. Keep generation invalidation and in-flight pause fencing separately. |
| `capture::transfer` | 2 → 1 | One transfer-policy table covers exact/over limits, binary bytes, blank content and an idle timeout before reading; retain budget-release assertions. |
| `capture::worker` | 3 → 2 | Extend the unavailable-engine lifecycle through pause, shutdown and rejected late resume. Retain sticky uncertain-shutdown testing independently. |
| `daemon::subscription` | 2 → 1 | Remove private parser-field assertions. Real JSONL/D-Bus subscription acceptance now checks empty/unknown stream rejection as well as fresh subscriber baselines. Keep outage/recovery state coverage. |
| `editor` | 2 → 1 | Replace command-vector inspection with actual process execution using spaces/metacharacters in paths. Keep invalid templates, output content and unsuccessful exit checks. |
| `ringboard` | 4 → 2 | Drop the storage-normalization microtest in favor of real main/favorite edit-without-echo capture checks. Remove the reimplementation of the hash formula; retain full-content identity, interrupted reads, bounded previews, length mismatches and JS-safe revisions. |
| `ringboard::artifacts` | 3 → 2 | Real backend checks already retain references beyond preview/file limits. Keep ownership/grace/active-selection/retry checks and exercise parsed references and ambiguous overlong lines in cleanup. Keep echo identity/reload coverage. |
| `ringboard::content` | 4 → 1 | Remove the `Read::take` wrapper and third-party image-limit API tests. Retain file parsing/cut/escaped names through resolved-content behavior. Actual PNG validation now rejects over-limit bytes and excessive dimensions. |
| `ringboard::mutation` | 5 → 4 | Remove the private map-removal/terminal-claim microtest. Cancellation and blocking-commit scenarios already verify exactly one terminal event and file cleanup. |
| `ringboard::operation` | 1 → 0 | Remove direct atomic-phase assertions and a duplicate timeout wait. The retained mutation scenarios exercise cancellation versus commit and wait for the real outcome. |
| `ringboard::screenshot` | 3 → 1 | Test geometry/cancellation through the selector process, rather than a separate parser test. Move unknown mode/field rejection to API validation. |
| `ringboard::search` | 2 → 1 | Remove repeated Unicode, invalid-tail and repeated-prefix checks. Keep the comparative short-read test, I/O errors after a match, and a long stream crossing the actual buffer boundary. |
| `selection` | 4 → 3 | Remove the mock publisher's echo/self-check scenario; real desktop checks cover MIME round trips. Keep aliases, bounded reads, rejection-before-publication and exact file-payload size including its operation header. |
| `session` | 3 → 2 | Drop private `paste_pending` field assertions. Failed-publication action tests and actual targeted-paste acceptance protect the behavior; keep expiry/end and unsafe-address checks. |
| `settings` | 5 → 4 | Check invalid retention limits through API validation instead of a separate manager fixture. Keep corrupt settings, failed persistence, privacy retry and abandoned-transition regressions. |

The API action-policy scenario no longer singles out the obsolete
`edit-external` action. It instead rejects a generic unknown action and opening a
non-file, without reserving a historical action name against future use.

## Validation and coverage trade-off

- **47 Rust tests pass; one existing benchmark remains ignored.**
- Strict all-target/all-feature Clippy and formatting pass.
- Rust Quality Lens `measure all`, `verify`, and partial/test-failure/practice-failure
  policy checks pass, without relaxing configuration.
- All **12 backend**, **12 capture**, and **15 standard nested-desktop** acceptance
  checks pass. Empty/unknown subscriptions are checked at the real service boundary.
- Shelllist layer-shell, wlr-only, multi-seat and physical-session qualification
  remain outside this pass; existing optional-tool skips and policy warnings remain.

Cargo/LLVM line coverage is **56.41% → 54.70%**. This includes test code, so removing
well-covered test bodies changes the denominator. Restricting the same line-hit
reports to the unchanged source prefixes outside inline test modules (excluding
the benchmark file) gives **2228/4582 → 2200/4582**, or **48.63% → 48.01%**.
The net loss primarily concerns subscription parsing, storage normalization,
bounded content reads and selection publication now exercised by real acceptance
checks rather than redundant unit tests. Those external runs are **not** included
in the Cargo coverage percentage; unchanged numerical coverage is not claimed.

Baseline source, test inventory and RQLens evidence are saved locally under
`target/test-reduction-baseline/`; current evidence is under `target/analysis/`.
`target/test-reduction-comparison.json` records the counts and line-hit differences.
The historical quality-review numbers describe the earlier refactor, not this
smaller test suite.
