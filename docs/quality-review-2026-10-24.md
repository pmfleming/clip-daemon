# Quality review: shared policy and denser tests

Baseline: `291148a`. Used local RustQualityLens at `../rust-quality-lens`
(`d28f3d9`), built from source and copied to `target/quality-20261024/rqlens` so
both runs use the same executable. Rust/Cargo 1.95; unchanged `rqlens.toml`.
Evidence and logs: `target/quality-20261024/{before,after}/`,
`comparison.json`, `metrics-after.log`, and `validation-final.log`.

## Remaining review findings

- **P2 — completed operation owners are retained.** `src/api.rs` inserts started
  operations into `operation_owners`, but removes them only on successful
  cancellation. Natural completion/failure has no removal path. A long-running
  daemon accumulates these records. Follow-up should give terminal ownership
  cleanup a single owner, including completion-before-registration and lost-event
  races; subscriber-driven cleanup alone would be insufficient. Not changed here.
- **P2 — platform/storage coverage remains weak.** This headless coverage run
  executes no lines in `src/ringboard.rs`, `src/ringboard/capture.rs`, or
  `src/capture/wayland.rs`. Fake-backend API tests are not evidence for those
  implementations. Real engine/compositor qualification remains necessary.

## Changes

- `FileSelection` now owns deserialization and path/count validation. API, fake,
  and Ringboard use the same validator; the duplicate API request structure,
  string-to-operation match, and repeated path checks are gone. Error codes stay
  stable; invalid-operation wording now comes from Serde, and backend path/count
  messages are normalized. The fake also rejects oversized/relative selections.
- Settings reconciliation, capture admission, and image mutations reuse
  `selection::effective_limit`, rather than repeating the hard-limit calculation.
  This removes settings/worker dependencies on backend constants and increases
  reuse of selection policy without adding modules or forwarding wrappers.
- Image inspection, decoding, and cached-thumbnail inspection share format
  sniffing and decode limits. Local-image identity lives on `LocalImageSource`,
  removing the artifact registry's direct selection-I/O dependency.
- Paste validation and arming share a locked expiry check; no check/use race or
  lock across asynchronous work was introduced. Hidden sessions still consume
  their state independently.
- Removed the unreachable reserved-method fallback: every method currently in
  the registry is dispatched. Wipe challenges move their ID into storage after
  constructing the response, avoiding one production string clone.
- API tests share envelope assertions and table cases. Added query/file bounds,
  malformed cursors, wipe challenge reuse, edit cancellation/details, expired
  paste validation/arming, and actual thumbnail decoding/cache assertions.
  Sensitive publication joins the publication contract matrix. The duplicate
  integration cursor-restart assertion was removed; the focused authenticated
  cursor test still owns process-key invalidation.

## Comparable results

| Signal | Before | After |
| --- | ---: | ---: |
| Function cognitive complexity, sum | 613 | 605 |
| Function cyclomatic complexity, sum | 1,797 | 1,796 |
| Function Halstead effort, sum | 4,353,166 | 4,351,879 |
| Mean observed-reuse leverage | 23.78 | 24.05 |
| Mean locality | 99.25 | 99.29 |
| Explicit `.clone()` calls, `src/` + `tests/` | 90 | 83 |
| Physical Rust lines, `src/` + `tests/` | 11,106 | 11,056 |
| Test functions, including ignored benchmark | 44 | 43 |
| Covered `src/` lines / instrumented lines | 4,015 / 7,373 | 4,113 / 7,358 |
| `src/` line coverage | 54.46% | 55.90% |
| Duplicate token groups | 11 | 11 |
| Production panic-path findings | 0 | 0 |
| Escape-hatch findings | 0 | 0 |

These are **incremental** gains, especially cyclomatic complexity and effort.
Six of the seven removed explicit clones are in integration tests; `src/` alone
falls 82 → 81. Test code volume is essentially flat while exercising more cases;
this is not a claim of a large test-suite reduction. No compiler-reported dead
code or unused dependencies remained to remove. Protocol callback duplication
was not hidden behind additional macros.

Structural measurements cover configured `src/` roots, including inline tests
and the benchmark, but not `tests/`. Coverage totals above are filtered to `src/`
and include inline test code; vendor/dependency/integration-test files are not
included. Halstead effort is a syntax metric, not elapsed engineering effort.
Architecture scores are advisory. Rust-analyzer resolved 100% of collected
references in both runs, but RQLens still marks inventory partial for unexpanded
Wayland macros. No exclusions, lint suppressions, or quality thresholds changed.

## Validation

Passed `cargo fmt --all --check`, `git diff --check`, locked all-target/all-feature
tests (**42 passed, 1 benchmark ignored**), strict all-target/all-feature Clippy,
doctests (none), and cargo-machete. Coverage used:

```sh
cargo llvm-cov --locked --all-features --workspace --json --output-path REPORT.json
```

RQLens producers: `hotspots`, `clones`, `escape-hatches`, `reliability`, `locality`,
and `leverage`. No live desktop qualification, benchmark, mutation campaign, or
security audit was run; the existing safety-containment boundaries remain intact.
