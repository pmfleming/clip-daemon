# Follow-up validation

## Step 3 — deterministic failure and concurrency coverage

Added tests for:

- failed copy/image-as-file publication never arming a real-target paste session;
- edit partial-publication results retaining the committed content, with single-use leases;
- a revision change during an edit rejecting the stale commit without overwriting new content;
- annotation cancellation both before readiness and while running, with exactly one terminal event and staged-file cleanup;
- expired/ended sessions never triggering paste;
- overlapping queries and eight competing replacements returning consistent snapshots, with only one writer succeeding against revision 1.

The fake backend previously checked replacement and bulk-delete revisions under a separate lock from mutation. It now validates and mutates under one write lock. Publication fault injection and synthetic paste-target helpers are compiled only in unit tests. No tests inject shortcuts into the real desktop.

`cargo test --all-targets --locked`: **39 passed**. The concurrent-query test also passed ten repeat runs. `cargo clippy --all-targets --all-features --locked -- -D warnings` passes. RQLens coverage reports **49.36% lines**, **47.98% functions**, and **48.02% regions** (versus 44.63% lines after the initial refactor).

Limitations: partial-publication API coverage uses an injected backend failure, not a compositor outage. Fake-backend atomicity is not a claim that Ringboard's multi-request IPC protocol offers compare-and-swap transactions against external capture/mutation. The live success/cancellation paths are exercised separately by the step-1 harness.

## Step 4 — remaining hotspot simplification

| Function | Hotspot before → after | Cognitive before → after | Cyclomatic before → after |
|---|---:|---:|---:|
| artifact pruning | 36.62 → 23.30 | 5 → 3 | 8 → 5 |
| action launching | 35.73 → 27.44 | 1 → 1 | 11 → 9 |
| entry inspection | 34.38 → 20.22 | 4 → 1 | 6 → 5 |

Pruning now takes a minimum age rather than a redundant force flag: `clear_all` already clears the active selection and can pass zero age. Failed file removals remain registered. Launch dispatch propagates one result instead of repeating propagation in every arm; file launches pass the original `OsStr` path rather than a lossy UTF-8 conversion. Inspection uses a bounded prefix read followed by streaming `io::copy` into SHA-256, retaining the full-content digest and checked size comparison without a manual chunk loop.

Tests cover forced pruning of young active files and independent full-stream digest equality across preview-boundary sizes and interrupted reads. **40 tests pass**, and strict all-feature Clippy passes. The function scores above come from fresh RQLens measurements immediately before/after this step, not changed thresholds or exclusions.

## Step 5 — dependency, security, and toolchain gates

- Declared Rust 1.95 in the manifest and pinned 1.95.0 in `rust-toolchain.toml`. The Nix shell actually executes rustc/cargo 1.95.0; compilation, all-feature tests, Clippy, doctests, and rustdoc pass on that version. Both active/pinned-toolchain and selected-dependency MSRV checks pass.
- Restricted the existing Nix `RUSTC_BOOTSTRAP=1` exception to `clipboard_history_core,clipboard_history_client_sdk`. The pinned 0.16.2 upstream crates still require an unstable borrowed-buffer feature. A completely exception-free stable build is not claimed.
- Enabled locked, all-target/all-feature RQLens verification, RustSec auditing, `cargo-machete`, and three repeat test runs. `just check` now includes dependency/audit checks; `just quality` measures, verifies, and enforces partial-input, test-failure, and practice-failure policies.
- `cargo-machete 0.9.2 --with-metadata` initially flagged `futures` and the intentionally pinned core dependency. Removed unused direct `futures`; use the pinned core API directly rather than ignoring the finding. The final scan reports no unused direct dependencies. No existing dependency versions were upgraded.
- RustSec database revision `bf25f6575a93a35f30796c65c0ed91bee7fa19fd` reports **zero known vulnerabilities**. It still reports **RUSTSEC-2024-0436**, unmaintained `paste 1.0.15`, through `stable-type → clipboard-history-client-sdk`. This advisory is not suppressed; removing it requires a separately reviewed upstream SDK/dependency change.

Final validation with `nix develop --command just check` and `nix develop --command just quality`: **40 passed, one explicitly ignored opt-in benchmark**, no failed/unknown tests; all three repeat runs pass. RQLens verification has 16 passed checks, zero failed errors, four project-documentation warnings, and eight skipped optional checks. Policy passes, production reliability findings and source escape-hatch findings are zero, and every module retains locality 100. The unexecuted benchmark and test-only fault injection contribute to static hotspot totals; they are not removed from measurement to improve scores.

All-feature coverage is **48.96% lines** / **47.81% functions** / **47.80% regions** across 20 files. This includes the intentionally unexecuted benchmark module, unlike step 3's default-feature coverage, so the percentages are not directly comparable.

The updated shell includes GTK introspection, the compositor/editor/clipboard tools, and cargo-machete. `just live-acceptance` passes all **11** isolated live checks; the launcher explicitly uses a short `/tmp` runtime path because Nix's longer `TMPDIR` exceeds Hyprland's socket-path limit. `just benchmark-history` re-ran successfully: identical allocation reductions and matching fingerprints, with median speedups of **2.11–3.47×** in that final run. The benchmark report now records working-tree state and a source SHA-256 in addition to the Git revision. `nix flake check --no-build` evaluates successfully; a full Nix package build was not run.

Remaining limits: actual Shelllist layer-shell and terminal-specific paste acceptance, capture-side sensitive/oversized-source policy, and externally concurrent Ringboard IPC transactions remain unqualified. Satty Escape currently emits a `failed` operation with a cancellation message and leaves the entry unchanged; explicit daemon operation cancellation emits `cancelled`. Optional mutation/fuzz/Miri/sanitizer/semver gates remain disabled. Contribution, conduct, security-reporting, and changelog documents are still reported missing. Ringboard and mutation modules still exceed the informational architecture-size threshold; no thresholds, audit ignores, or lint suppressions were added.
