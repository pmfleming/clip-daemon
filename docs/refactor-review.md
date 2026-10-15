# Focused Rust quality review

Baseline: `ba2163b`; local Rust Quality Lens checkout `7461a91`, Rust 1.95.
Measured `hotspots`, `clones`, `escape-hatches`, `reliability`, `map`,
`locality`, and `leverage` with the existing `rqlens.toml`. No exclusions,
thresholds, waivers, or dependencies changed. Baseline architecture evidence
was regenerated against an isolated copy of the baseline commit.

Raw evidence: ignored `target/refactor-before/` and `target/refactor-after/`.
Function aggregates below include inline tests and the opt-in benchmark.
Halstead effort is the tool's syntax-token metric, not developer hours.

| Signal | Before | After |
| --- | ---: | ---: |
| Sum of cognitive complexity | 562 | 560 |
| Sum of cyclomatic complexity | 1,739 | 1,735 |
| Sum of Halstead effort | 4,179,856 | 4,137,596 |
| Sum of function hotspot scores | 5,733.91 | 5,701.38 |
| Edit commit cognitive / cyclomatic | 5 / 13 | 2 / 7 |
| Edit commit Halstead effort | 47,473 | 20,624 |
| Average observed-reuse leverage | 22.73 | 23.33 |
| Ringboard leverage | 60 | 70 |
| Ringboard locality | 82 | 88 |
| Average locality | 99.32 | 99.32 |
| Duplicate token groups / lines | 8 / 145 | 6 / 103 |
| Production panic-path findings | 1 | 0 |
| Escape-hatch findings | 0 | 0 |
| Direct `.clone()` sites in `src/` (including tests) | 97 | 96 |
| Physical Rust lines in `src/` (including tests) | 10,340 | 10,320 |

## Changes

- `src/actions.rs`: put consumed edit-lease validation/replacement on the lease,
  share paste-target validation, and share repeated edit-test setup. Revision
  revalidation, single use, size limits, and publication-before-paste remain.
- `src/backend.rs`: centralize invalid-data and operation-error construction;
  remove seven duplicate local wrappers. These are crate-private helpers, not
  new public API. Compiler dead-code checks and cargo-machete were already
  clean; this removes redundant scaffolding, not known unreachable features.
- `src/ringboard/content.rs`: own content inspection, details, thumbnails, PNG
  validation, and their tests. Match the image representation once when
  resolving echoes. Move file previews into details instead of cloning them;
  borrow the existing thumbnail file instead of duplicating its descriptor.
  Remove the single-consumer `Publication` enum and forwarding method.
- `src/ringboard/screenshot.rs`: colocate explicit and interactive captures,
  and share bounded-PNG publication and selection invalidation. Retain timeout,
  temporary-file cleanup, transaction, and commit/cancellation boundaries.
- `src/daemon/subscription.rs`: borrow operation status and replace the explicit
  serialization `expect` with an error event. Add a maximum-u64 lag-count wire
  regression test.
- Reset summary caches through their existing default state rather than
  repeating their fields.

## Validation and tradeoffs

Passed formatting, `git diff --check`, all-target/all-feature Clippy with
`-D warnings`, cargo-machete, all-target/all-feature tests, and doctests:
**56 tests passed**, with the opt-in benchmark ignored in the normal run.
The benchmark was then run explicitly on both revisions: all **12** history
result fingerprints, allocation counts, and allocated-byte counts match.

An attempted fused pagination/map implementation saved one allocation but
increased allocated bytes; it was reverted rather than accepted on static
metrics alone. No runtime speedup is claimed.

Global complexity maxima are unchanged (cognitive 9, cyclomatic 15). The
Ringboard adapter's locality improves, but content and screenshot locality
fall from 100 to 97 as they acquire their cohesive responsibilities; average
locality is unchanged. The content file is larger, not a claim of universal
module-size improvement. Remaining token duplication is low-risk, not a reason
to add macros or suppressions.

RQLens evidence remains **partial**: rust-analyzer resolved all collected
architecture references, but unexpanded Wayland macros and cross-file impl
ownership limit the authored-syntax inventory. These are heuristic comparisons,
not complete correctness proof. Live Wayland/Ringboard/desktop acceptance,
coverage, audit, and optional advanced verification were not run in this pass.
The SDK panic-containment boundary was intentionally retained.
