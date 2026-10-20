# Quality refactor follow-up

Baseline: `3cdca2c`; local RustQualityLens (`../rust-quality-lens`, `7461a91`),
Rust 1.95. Measured `hotspots`, `clones`, `escape-hatches`, `reliability`,
`locality`, and `leverage` before and after with the unchanged `rqlens.toml`.
Evidence is retained in ignored `target/refactor-baseline/` and
`target/refactor-after/`; projection runs are `target/refactor-*-bench.log`.

## Results

Function totals include inline tests and the opt-in benchmark. Halstead effort
is a syntax metric, not estimated developer time. Architecture scores are
advisory, not correctness gates.

| Signal | Before | After |
| --- | ---: | ---: |
| Cognitive complexity, sum | 560 | 555 |
| Cyclomatic complexity, sum | 1,719 | 1,712 |
| Function Halstead effort, sum | 4,162,365 | 4,130,171 |
| Function hotspot scores, sum | 5,732.32 | 5,687.14 |
| Average locality | 99.32 | 99.41 |
| Average observed-reuse leverage | 23.33 | 23.64 |
| Direct `.clone()` calls in `src/` | 89 | 80 |
| Physical Rust lines in `src/`, including tests | 10,096 | 10,078 |
| Duplicate token groups / lines | 6 / 103 | 6 / 103 |
| Production panic-path findings | 0 | 0 |
| Escape-hatch findings | 0 | 0 |

These are incremental improvements to an already repeatedly refactored project.
Compiler dead-code checks and cargo-machete were clean at baseline; no unused
feature or dependency was invented to remove. Redundant scaffolding was removed
instead. Remaining duplication does not justify introducing more macros.

## Review findings and changes

- **Staging cleanup could delete an existing file on create failure.**
  `src/ringboard/mutation.rs::write_private` now only removes a file it successfully
  created. A regression test checks that an existing file is neither overwritten
  nor deleted. Text replacements also reuse this helper, cleaning up partial writes.
- **Subscription state was cloned despite sharing one task lifetime.**
  `src/daemon/subscription.rs` borrows the emitter, subscription ID, API service,
  and stream list, removing the duplicate lifecycle subscription struct. History
  failures remain typed across `src/api.rs` and the subscription instead of being
  serialized, cloned, deserialized, and potentially replaced by a fallback error.
  Subscription registration still precedes acknowledgement.
- **Editor configuration was deep-cloned with every backend clone.**
  `src/ringboard/operation.rs` now owns the editor alongside operation lifecycle
  and shared staging helpers. The backend can derive `Default`; screenshot code
  no longer depends on mutation code for file staging.
- **Redundant ownership and I/O.** `ResolvedContent` borrows its stored MIME.
  Bounded path reads live in `src/selection.rs` and serve publication, annotation,
  and artifact validation. Annotation publication consumes the bytes already read
  and registered, instead of reopening the file and allocating another buffer.
- **Redundant cache state.** A cache token is installed only after its projection
  loads successfully. There is no second optional projection or impossible
  missing-projection error path. Failed loads, incomplete projections, and search
  errors still invalidate the cache. `project_query` drops from cognitive/cyclomatic
  4/9 to 3/6, and effort from 34,745 to 28,374.
- **Shutdown backoff polling.** Capture reconnect uses `park_timeout`, and closing
  the worker unparks it. This removes the nested 25 ms sleep loop while preserving
  the submission fence. Reconnect drops from cognitive/cyclomatic 8/6 to 5/4,
  and effort from 26,492 to 17,507.

Locality improves for Ringboard (88 → 91) and screenshots (97 → 100), while
mutation locality falls (97 → 94) because bounded reads now have one shared owner.
Selection leverage rises from 20 to 40. Mutation's observed external reuse falls
from 10 to 0 after staging moves; lower reuse for a private implementation module
is not evidence that it should acquire artificial callers.

## Validation and limits

Passed formatting, strict all-target/all-feature Clippy, cargo-machete,
all-target/all-feature tests, doctests, and `git diff --check`: **40 tests passed**,
with the projection benchmark ignored in the normal run and then run explicitly.
All **12** baseline/final projection fingerprints, allocation counts, and allocated
byte totals match. These were debug allocation/semantic checks, not release-speed
measurements. A fused-pagination experiment increased allocated bytes, and a
streaming-preview-matcher experiment substantially slowed debug benchmarks; both
were reverted rather than accepted on static metrics alone.

Rust-analyzer resolved all collected dependency references in both runs. RQLens
evidence remains partial because authored syntax does not fully inventory Wayland
macros and cross-file impl ownership. No exclusions, waivers, dependencies,
or thresholds changed. SDK panic containment remains intentional. Live desktop,
Wayland/Ringboard acceptance, coverage, and security auditing were not rerun.
