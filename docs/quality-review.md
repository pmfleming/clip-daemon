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

Generated JSON remains under ignored `target/analysis/` and is intentionally not committed.
