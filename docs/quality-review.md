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

## Failure-path remediation follow-up

The 2026-08-15 review added post-publication paste arming, explicit partial edit-publication results, bounded screenshot execution, lossless JSONL EOF handling, and subscription lag recovery. Regression coverage now includes the JSONL executable boundary and the affected URL, screenshot, paste-session, and publication contracts.

Coverage reports 49.22% of lines, 47.98% of functions, and 47.66% of regions across 19 Rust files. The JSONL client increased from 0% to 56.38% line coverage, session policy reached 58.57%, and Ringboard mutation coverage increased to 18.01%. All 43 tests pass. RustSec reports no known vulnerabilities; the pinned Ringboard SDK still transitively uses the unmaintained `paste` crate through `stable-type`.

## Cognitive and architecture pass

The follow-up refactored lifecycle/subscription dispatch, JSONL request processing, paste feedback, annotation publication, file URI encoding, settings restarts, selection options, and artifact persistence into smaller policy boundaries. Repeated test fixtures and publication assertions were consolidated without reducing coverage.

| Signal | Before | After |
|---|---:|---:|
| maximum function hotspot | 73.92 | 40.21 |
| maximum module hotspot | 70.93 | 42.88 |
| aggregate function effort | 3616.54 | 3491.91 |
| aggregate branch pressure | 633 | 623 |
| functions scoring at least 35 | 20 | 12 |
| minimum locality | 97.0 | 100.0 |
| average locality | 99.66 | 100.0 |
| minimum leverage | 50.0 | 53.0 |
| average leverage | 66.85 | 67.20 |
| clone records | 19 | 14 |
| measured Rust SLOC | 6,174 | 6,163 |
| physical Rust source lines | 6,817 | 6,815 |

Escape hatches remain at zero. Production reliability findings fell from one checked-fixture panic path to zero, direct `.clone()` calls fell from 75 to 72, and `.cloned()` calls fell from three to one. All 43 tests pass; coverage reports 48.69% of lines, 47.80% of functions, and 47.76% of regions, with 51.02% changed-line coverage.

## Shared-client and hotspot follow-up

The follow-up after `0937549` retained unbounded JSONL call draining while reusing the shared output actor and D-Bus transport, bounded concurrent requests, separated query collection from projection installation, and isolated cache cleanup, wipe, and local-image inspection policy.

| Signal | Before | After |
|---|---:|---:|
| maximum function hotspot | 39.59 | 36.96 |
| maximum module hotspot | 40.55 | 37.88 |
| aggregate function effort | 3,502.21 | 3,441.03 |
| aggregate cyclomatic complexity | 1,227 | 1,225 |
| aggregate cognitive complexity | 304 | 299 |
| functions scoring at least 35 | 8 | 2 |
| JSONL client module | 40.55 | 30.03 |
| Ringboard mutation module | 39.54 | 33.87 |
| Ringboard content module | 36.62 | 30.65 |
| client leverage | 64.5 | 67.5 |
| average leverage | 67.225 | 67.250 |
| production `.clone()` calls | 71 | 69 |
| measured nonblank Rust lines | 6,243 | 6,233 |
| physical Rust lines | 7,151 | 7,148 |

Locality remains at the maximum 100 for every module, escape-hatch count remains zero, and the three remaining clone findings are low-risk token windows with a maximum score of 10. All 45 unit and integration tests pass with no failed or unknown results.

## Borrowed projection and policy-locality review

Fresh measurements against `fc1ac57`, using the local `../rust-quality-lens` checkout (Rust 1.95). These compare the same current extractor and configuration, rather than the historical test counts above. Raw evidence is in ignored `target/analysis-before-refactor/` and `target/analysis/`.

| Signal | Before | After |
|---|---:|---:|
| aggregate function effort (sum of hotspot scores) | 3,461.73 | 3,411.43 |
| aggregate cognitive complexity | 302 | 296 |
| aggregate cyclomatic complexity | 1,211 | 1,199 |
| maximum function hotspot | 37.46 | 36.62 |
| maximum module hotspot | 40.04 | 39.27 |
| minimum / average leverage | 53 / 67.200 | 56 / 67.225 |
| minimum / average locality | 100 / 100 | 100 / 100 |
| duplication records / duplicated lines | 4 / 54 | 3 / 42 |
| production direct `.clone()` calls | 62 | 51 |
| production physical Rust lines (excluding inline tests) | 6,062 | 5,954 |
| physical Rust lines including all tests | 7,161 | 7,136 |
| RQLens source nonblank lines | 6,158 | 6,127 |
| measured line coverage | 43.13% | 44.63% |

Changes and review findings:

- **History queries:** borrow cached candidates, IDs, and summaries instead of deep-cloning the entire projection on every request. Clone summaries only for the page and current entry. Identity bindings still cover all visible history, and generated-file references include collapsed echoes. Thumbnail cleanup reuses the bindings instead of maintaining another owned ID list.
- **API and deletion policy:** remove the duplicate routing enum, deserialize bulk selections directly into typed backend targets, and validate duplicate IDs by reference before any deletion. Text publication policy lives in the action service; decoding still precedes the settings lookup.
- **Editor locality:** put process execution and process-group cleanup beside the editor command adapter and its tests. Remove the single-use generic operation-launch layer, redundant editor cloning, unused private `Clone` implementations, and forwarding helpers. Preserve cancellation ownership and partial annotation-publication results.
- **Other allocations:** retain artifact records in place, borrow file URIs and canonical MIME strings, return saved settings from the blocking task by ownership, and clone only paginated fake-backend summaries. Bounded reads use `Read::take`; thumbnail result construction is shared between cache hits and newly generated images.

The expanded table-driven query test includes the former echo-collapse test and covers filtering, pagination, current-entry flags, orphan echoes, identity bindings, and incomplete projections. Other regression cases cover artifact grace periods, active/referenced files, failed deletion retries, missing files, bounded-read position/EOF, malformed bulk selections, and strict publication decoding. All **32** current unit/integration tests pass; no tests are failed or unknown.

Validation:

```sh
cargo clippy --all-targets --locked -- -D warnings
../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
../rust-quality-lens/target/debug/rqlens verify --config rqlens.toml
../rust-quality-lens/target/debug/rqlens check --config rqlens.toml --fail-on partial --fail-on test-failure
```

Formatting, compilation, Clippy, tests, doctests, and rustdoc pass. Escape-hatch and production reliability findings remain zero. Locality was already at the tool's ceiling; it is preserved, not claimed as improved. Artifact pruning's individual hotspot rises from 30.80 to 36.62 in exchange for removing the temporary cloned-path collection; aggregate complexity and effort still fall. Remaining duplication findings are low-risk token windows, not justification for more macros.

Review limitations: these are static/coverage results, not measured runtime speedups or live Wayland/Ringboard acceptance. The SDK panic-containment boundary remains intentional. RQLens still reports missing MSRV, contribution, conduct, security-policy, and changelog declarations; optional audit, unused-dependency, mutation, and other advanced gates were not enabled. The informational architecture threshold remains exceeded by the Ringboard adapter and mutation module; no thresholds or exclusions were relaxed.
