# Rust quality review

Measured after phases 1 and 2 with the sibling `rust-quality-lens` checkout:

```sh
nix develop --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
nix develop --command ../rust-quality-lens/target/debug/rqlens review --changed-since 8228173 --config rqlens.toml
```

## Refactoring results

The phase-1 baseline had these largest non-entrypoint function scores:

| Function | Before | After |
|---|---:|---:|
| `client::run` / request loop | 81.87 | 29.34 |
| subscriber history polling | 71.34 | 19.43 |
| bounded preview normalization | 66.81 | 0.00 |
| Ringboard history query | 61.82 | 20.50 |
| semantic classification | 59.47 | 0.00 |

The refactor separates JSONL request decoding/dispatch, isolates subscription change emission, uses small classification transformations, centralizes selected-entry loading, and extracts Ringboard content preview/thumbnail policy from database traversal. `ringboard.rs` fell from 400 to 290 physical lines. Despite adding the content module and tracked quality configuration, total Rust source lines fell from 1,778 to 1,776.

Architecture observations:

- locality remains at the tool's maximum score of 100 for every module;
- leverage improved for the shared boundaries: `lib` 88.0 → 90.5, `backend` 72.0 → 77.0, `model` 78.0 → 80.5, `classification` 64.5 → 67.0, and the Ringboard adapter 56.0 → 58.5;
- escape-hatch count is zero;
- type-health reports no structural-risk types;
- clone findings are only low-risk token windows (maximum score 15); the repeated details/thumbnail selected-entry transaction was removed;
- all eight discovered tests pass and correctness extraction reports no failed or unknown tests.

## Phases 3 and 4 follow-up

The mutation/privacy implementation was reviewed again at `242c025`. The follow-up replaced the backend's repeated mutation methods with one typed mutation boundary, separated Satty staging from execution, and moved API contract coverage to an integration-test layer.

| Signal | Before | After |
|---|---:|---:|
| entry-action function score | 110.10 | 60.93 |
| annotation function score | 57.96 | 27.10 maximum across staged annotation functions |
| fake mutation function score | 54.15 | 21.73 |
| token-clone records | 71 | 53 |
| minimum module locality | 97.0 | 100.0 |
| API leverage | 57.5 | 60.5 |
| Rust source lines | 2,822 | 2,783 |
| all Rust lines including integration tests | 2,822 | 2,814 |

Escape hatches remain at zero, maximum clone score remains low at 15, and all 11 tests pass across two test layers.

## Phase 5 follow-up

The phase-5 baseline at `21b58f0` was reviewed after the intelligent-action surface was complete. Related edit, launch, validation, and API error policy was consolidated behind one action service, while repeated entry load/revision checks were replaced by a shared boundary.

| Signal | Before | After |
|---|---:|---:|
| API dispatch function score | 91.67 | 30.02 |
| entry-action facade score | 74.54 | 8.29 |
| maximum action execution score | 74.54 | 61.82 |
| API module score | 79.98 | 32.48 |
| token-clone records | 117 | 115 |
| minimum module locality | 91.0 | 97.0 |
| API leverage | 51.5 | 57.5 |
| Rust source lines | 3,244 | 3,224 |
| all Rust lines including integration tests | 3,340 | 3,320 |

Escape hatches remain at zero and maximum clone score remains low at 15. All 12 unit/integration tests pass.

## Current quality pass

A full Rust Quality Lens pass after the phase-5 refactor reduced the largest function score from 61.82 to 31.58 and the largest module score from 52.82 to 35.64. The Ringboard mutation module fell from 45.07 to 32.31, settings from 42.74 to 31.05, and the executable module from 44.01 to 23.03.

The API now delegates session policy to the action service, raising API locality from 97.0 to 100.0 and leverage from 57.5 to 60.5. Token-clone records fell from 115 to 41, escape hatches remain at zero, and Rust source lines fell from 3,224 to 3,203. All 12 tests pass with no unknown results.

Generated JSON remains under ignored `target/analysis/` and is intentionally not committed.

## Subscription, query, and coverage follow-up

The 2026-07-25 follow-up added deterministic subscription state tests, isolated subscription task lifecycle and change-state transitions, and moved Ringboard query bookkeeping into `QueryAccumulator`. The development shell now includes `cargo-llvm-cov` and matching LLVM tools, so Rust Quality Lens coverage is complete rather than partial.

| Signal | Before | After |
|---|---:|---:|
| maximum function hotspot | 45.86 | 37.69 |
| subscription startup | 45.86 | 3.92 |
| Ringboard history query | 44.91 | 30.74 |
| subscription history polling | 43.62 | 18.03 |
| aggregate function effort | 2661.65 | 2618.81 |
| functions scoring at least 35 | 7 | 4 |
| clone records | 39 | 39 |
| average locality | 99.83 | 99.83 |

Coverage now reports 40.48% of lines, 42.51% of functions, and 40.19% of regions across 17 Rust files. All 30 discovered tests pass with no failed or unknown results, and escape-hatch count remains zero. The enabled partial-input and test-failure policies pass; the architecture map still reports the Ringboard module's aggregate 615.7 score above the informational 600 threshold.

## Clipboard publication quality pass

The pass after `6802929` consolidated API response/limit policy, reused the D-Bus client transport for stdin publication, split query projection from candidate collection, centralized editor-task startup and artifact locking, made MIME aliases data-driven, and reused file-backed replacement/content-resolution boundaries.

| Signal | Before | After |
|---|---:|---:|
| maximum function hotspot | 81.65 | 44.35 |
| maximum module hotspot | 65.23 | 45.56 |
| aggregate function effort | 3595.30 | 3354.26 |
| aggregate branch pressure | 633 | 597 |
| functions scoring at least 35 | 19 | 13 |
| functions scoring at least 50 | 6 | 0 |
| minimum locality | 94.0 | 97.0 |
| average locality | 99.51 | 99.66 |
| minimum leverage | 47.0 | 50.0 |
| clone records | 20 | 18 |
| production Rust lines | 6,372 | 6,366 |

Escape hatches remain at zero. Coverage reports 45.15% of lines, 44.16% of functions, and 44.05% of regions. All 38 discovered tests pass with no failed or unknown results.
