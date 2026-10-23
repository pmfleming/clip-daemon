# Quality review: selection ownership and content locality

Baseline: `60a6340`; local RustQualityLens `../rust-quality-lens` at `7461a91`,
Rust 1.95. Ran `hotspots`, `clones`, `escape-hatches`, `reliability`, `locality`,
and `leverage` with the unchanged `rqlens.toml`. Ignored evidence snapshots are
`target/review-baseline/` and `target/review-after/`.

## Results

Totals include inline tests and the opt-in benchmark, not just production code.
Halstead effort is a syntax metric, not time. Architecture scores are heuristic.

| Signal | Before | After |
| --- | ---: | ---: |
| Function cognitive complexity, sum | 620 | 611 |
| Function cyclomatic complexity, sum | 1,800 | 1,794 |
| Function Halstead effort, sum | 4,337,589 | 4,329,078 |
| Average observed-reuse leverage | 22.86 | 23.33 |
| Average locality | 99.44 | 99.29 |
| Direct `.clone()` calls in `src/` | 83 | 80 |
| Physical Rust lines in `src/`, including tests | 10,544 | 10,541 |
| Duplicate token groups | 11 | 11 |
| Production panic-path findings | 0 | 0 |
| Escape-hatch findings | 0 | 0 |

These are incremental gains; **not every requested metric improved**. Locality
falls because actions now depend on selection validation and Ringboard capture
explicitly depends on content helpers (both modules score 100 → 97). Content
leverage rises 40 → 50, selection 40 → 60; the shared poller scores 20. Kept these
real reuse relationships rather than adding forwarding wrappers to hide edges.

## Changes and findings

- **Duplicated Wayland polling:** `src/wayland_io.rs` owns flush/prepare/poll/read
  for capture and sensitive ownership. It preserves the 25 ms wakeup, bounded
  stack storage, transfer wakeups, interrupted-poll handling, and read-guard
  release. No extra macro, unsafe code, or heap allocation was introduced.
  Sensitive `run_on` cognitive/cyclomatic complexity falls **20/18 → 15/14**;
  effort falls **82,475 → 53,479**. Capture `run` falls **8/11 → 6/8**.
- **Duplicated sensitive validation:** one selection-domain validator serves
  API validation and native publication. `SelectionService` also checks it
  before invoking an injected publisher. Invalid UTF-8, byte lengths, and expiry
  limits stay fail-closed; valid requests still have no ordinary-publisher fallback.
- **Scattered content identity:** MIME loading, hashing, fingerprints, and
  revision construction now live with content interpretation and its tests in
  `src/ringboard/content.rs`. Fingerprint domains and algorithms are unchanged.
- **Unnecessary ownership:** image-file preparation borrows existing paths with
  `Cow<Path>`; only newly created files carry owned paths. Two large test buffers
  are mutated after their original assertions instead of cloned.
- **Redundant dispatch:** `src/api.rs` routes capture methods once, eliminating
  a second match and its unreachable fallback. SDK panic containment remains;
  `read_summary` simply returns its match result directly.

Compiler dead-code checks and cargo-machete found no unused code/dependencies to
remove. Escape-hatch findings were already zero. Remaining protocol callback
similarity and necessary ownership clones were not replaced with extra macros or
more complicated lifetime machinery merely to improve counts.

## Validation and limitations

Passed formatting, strict all-target/all-feature Clippy, all-target/all-feature
tests (**42 passed, 1 benchmark ignored**), doctests (none), cargo-machete, and
`git diff --check`. Logs are `target/review-{after-tests,clippy,doctests,machete}.log`.
The added poller test covers invalid descriptor capacity, transfer-only wakeups,
timeout, and disconnect. Existing in-process compositor expiry/owner-safety and
capture-fence tests pass. No live desktop qualification or performance benchmark
was run.

Rust-analyzer resolved all collected dependency references in both snapshots.
RQLens still marks inventory evidence partial for Wayland macros and cross-file
impl ownership. A measurement rejected changing inputs while the sibling
`daemon-framework` checkout was being edited independently; it was rerun
successfully. Those sibling changes were not modified or reverted here. No
exclusions, lint suppressions, dependencies, or quality thresholds were changed.
