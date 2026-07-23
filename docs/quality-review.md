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

Generated JSON remains under ignored `target/analysis/` and is intentionally not committed.
