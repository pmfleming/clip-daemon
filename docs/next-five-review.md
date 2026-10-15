# Next five quality improvements

Baseline: `35498c7` (the preceding refactor checkpoint). Each improvement was
implemented and verified before its own commit. Local Rust Quality Lens
`7461a91` supplies static evidence; configuration and thresholds are unchanged.

1. **Consistent Unicode search** (`src/ringboard.rs`, `src/ringboard/search.rs`).
   Preview, MIME, query, and streamed-content matching now use the same
   character-wise lowercase mapping. Greek sigma cases cover the previous
   disagreement with contextual `str::to_lowercase`. An ASCII fast path and
   preallocated Unicode output avoid introducing allocation growth.
2. **Stop matching after a hit** (`src/ringboard/search.rs`). KMP no longer
   examines the rest of a matched value, but UTF-8 validation and reads still
   continue through EOF. Coverage includes malformed tails beyond a full read
   buffer, short reads, interruptions, and I/O failure after a match.
3. **Allocation-free bounded polling** (`src/capture/wayland.rs`). Poll the
   connection and at most four transfers using a fixed stack array. A pipe-based
   test checks source versus transfer readiness, an empty transfer list, explicit
   overflow rejection, and zero heap allocations without requiring Wayland.
4. **Blank fuzzy-query fast path** (`src/history_search.rs`). Empty and whitespace
   queries preserve the original catalog: distinct recency scores already define
   their order. Tests check empty and populated catalogs, Unicode whitespace,
   all returned fields, and zero ranking allocations. Cursor checks are unchanged.
5. **Value-semantic settings snapshots** (`src/settings.rs`). Scalar-only
   `ClipboardSettings` is now `Copy`; remove three redundant clone calls and use
   struct update to retain untouched preferences. Test that updates preserve
   capture/privacy preferences and leave the previous snapshot unchanged. This
   is clearer value semantics, not a claim of a heap-allocation improvement.

## Verification

Every step passed all-target/all-feature tests, Clippy with `-D warnings`, and
formatting. Final verification also passed doctests, cargo-machete, and
`git diff --check`. **59 tests pass**, with the opt-in history benchmark ignored
in the ordinary run and executed separately. Its **12** result fingerprints,
allocation counts, and allocated-byte totals match the baseline.

The initial Unicode implementation increased allocation counts by repeatedly
extending an initially empty String. The benchmark caught this; the correction
is included in the Unicode commit. No timing-based speedup is claimed.

Evidence is ignored under `target/next-five-v2-before/` and
`target/next-five-v2-step1/` through `target/next-five-v2-step5/`. The step-1
`fold-*` logs record the allocation correction; the step-5 artifacts are the
final combined measurements.

## Static-metric tradeoffs

These increments prioritize a correctness fix, bounded allocation reductions,
and explicit snapshot semantics. They are **not** an aggregate complexity or
LOC reduction: additional guards and regression coverage increase total
cognitive complexity from 560 to 567, cyclomatic complexity from 1,735 to 1,745,
and Halstead effort from 4,137,596 to 4,217,693. KMP `feed` itself falls from
7,962 to 6,342 Halstead effort. Average locality (99.32), leverage (23.33), and
six duplicate-token groups are unchanged. Direct `.clone()` sites in `src/`
fall from 96 to 93. Escape hatches and production panic-path findings remain
zero. No thresholds, exclusions, or waivers were changed to disguise costs.

RQLens still marks the syntax inventory partial because of generated Wayland
macros and cross-file impl ownership. The new focused tests do not substitute
for live Wayland/Ringboard acceptance, which was not run.
