# Capture migration progress

Implementation follows [the migration plan](wayland-capture-migration.md).
Production services are not changed by these development commits.

## Stage 1 — contract and design

- Accepted ADR 0003 with explicit regular-only capture, startup/resume semantics,
  ext/wlr protocol selection, privacy barriers, budgets, and licensing boundaries.
- Added 14 executable SDK MIME fixtures, including sensitive-marker ordering,
  GNOME cut payloads, text aliases, image priority, and Chromium exclusions.
- Enabled the SDK's Apache-2.0 watcher-utils API; no copied upstream source.
- Validation: `cargo test --offline --test capture_contract` passed. Existing
  backend/desktop suites remain the integration baseline for later stages.
- Real compositor fallback, multi-seat, and deployment qualification are future
  gates, not claimed by this stage.

## Stage 2 — lifecycle and policy seams

- Added `CaptureControl` injection to settings/API, keeping a transitional systemd
  adapter so the packaged collector is unchanged at this stage.
- Added a closed-by-default generation gate and serialized submission fence.
  Old queued work cannot submit after pause/resume; ambiguous sink outcomes close
  admission and prohibit a false verified-pause or blind retry.
- A failed pause-settings write now attempts to stop local capture, returns the
  persistence error, and never claims durable/verified privacy.
- Tests cover initial closure, stale generations, blocked submission versus pause,
  uncertain replies, repeated transitions, and persistence failure.
- Validation: formatting and strict all-target/all-feature Clippy passed; 54 tests
  passed, with one opt-in benchmark ignored.
- Work continues on branch `capture-migration` in the sibling worktree
  `clip-daemon-capture` to avoid unrelated concurrent screenshot changes on main.
  Actual transfer cancellation and Wayland lifecycle remain stage 3/4 gates.

## Stage 3 — transport and atomic ingestion

- Added an event-driven ext/wlr collector, regular-selection-only capture, full
  offer exclusion, bounded memory-backed transfers, deadlines, bounded ingest
  queue, and acknowledgement-based pause/join. Bootstrap selections are skipped
  after pause/reconnect, using a compositor sync barrier rather than a timer.
- Added policy protocol v2 (`0xc2`, operation 8): the server admits and hashes a
  bounded snapshot, then atomically promotes a matching candidate in its original
  ring or adds to main. Safe rejection and uncertain outcomes are distinct.
- Real desktop qualification exposed a panic in the upstream SDK deduplicator's
  BorrowedBuf helper. Replaced that dependency with bounded positional comparisons
  using the existing backend read boundary. No new unsafe Rust or copied SDK code.
  A 128 MiB candidate-I/O budget may intentionally fall back to Add under a
  pathological comparison workload; no unchecked promotion is used.
- Validation: strict Clippy and formatting passed; 62 Rust tests passed (one opt-in
  benchmark ignored); all 12 policy backend scenarios passed. The disposable
  nested-Hyprland collector test passed capture/dedup, primary exclusion, pause
  fencing/no replay, sensitive exclusion, and exact/over-limit admission.
- This stage's collector is accessible through the disposable `capture-worker`
  test driver only; production still uses the packaged watcher. Settings/wipe
  coordination and full existing desktop round trips are stage 4 integration
  gates. wlr-only compositor and multi-seat hardware qualification remain pending.

## Stage 4 — integrated policy and recovery

- Added opt-in `daemon --capture-in-process` for disposable qualification; the
  packaging default is unchanged until stage 5. D-Bus ownership is acquired before
  capture initialization, and daemon teardown permanently fences its collector.
- Settings changes quiesce before changing retention/byte limits, verify live
  engine readiness instead of sleeping a fixed delay, and keep capture stopped on
  saved-but-unapplied policy. Resume requires synchronized engine limits.
- Wipe holds the settings/capture transition lock while draining old submissions
  and mutating history. Failed fences prevent deletion. Failed recovery after a
  committed wipe is a warning, not a false claim that wipe itself failed.
- API-only constructors no longer control external watchers. Headless regression
  tests exercise real in-process pause/unavailable-state handling, not fake watcher
  systemctl state. Cancelled transitions remain conservatively unverified.
- Validation: formatting and strict Clippy passed; 65 Rust tests passed (one
  opt-in benchmark ignored); all 12 backend scenarios passed. Ten integrated
  collector checks passed, including private restart, corrupt-settings startup,
  retention failure/recovery, wipe/no recapture, and compositor reconnect. All 15
  standard nested desktop checks passed, including GTK/terminal paste, Satty,
  images, multi-file payloads, sensitive exclusion, and bulk deletion.
- The two optional real-Shelllist layer-shell checks were not run in this stage;
  neither wlr-only/multi-seat hardware nor production activation is claimed.
- Per updated instruction, this and subsequent stages are committed locally only.

## Stage 5 — two-unit packaging and declarative cutover

- `clip-daemon daemon` now owns capture by default. Removed the migration flag,
  external-watcher control adapter, `capture-allowed` command and packaged watcher
  unit. Retained the legacy engine/watcher build for the rollback window only.
- The package ships facade + notify-ready engine units. The facade conflicts with
  and orders shutdown of a legacy watcher, while remaining independent of engine
  restarts so it can report retention failures.
- Prepared Home Manager's two-unit mapping and a `/dev/null` mask for the retired
  collector in the isolated `nixos-capture` worktree. No live units were changed.
- Added deliberate backup/cutover/rollback instructions. Fresh unit linkage does
  not overwrite existing files; no test or migration command activates production.
- Validation: strict Rust checks/65 tests and all ten integrated collector checks
  passed with the default daemon command. The Nix package built successfully.
  Installed-package smoke verified two units, conflict ordering, private startup,
  D-Bus activation and v2 negotiation. Synthetic history/private intent survived
  previous-engine rollback followed by upgrade. This is not a real systemd VM test.
- Nix configuration evaluation verified two enabled packaged services, no duplicate
  declarations, no watcher enablement and a mask resolving to `/dev/null`.
  Nix formatting, deadnix and statix checks passed after formatting the new mapping.

## Stage 6 — qualification and documentation (release gates remain open)

- Integrated the separately committed screenshot work from main into the isolated
  migration branch without modifying the original checkout.
- A new live regression reproduced duplicate entries after inline text edits.
  Capture and replacement now share Ringboard's plain-text MIME normalization;
  main/favorite edits preserve one history entry. Other MIME identities and server
  proof validation remain unchanged.
- Tightened the tracked-offer cap to include transfers, released seat proxies on
  removal, and report unavailability after losing the last seat.
- Added a real 64 MiB boundary check, metadata-only CPU/RSS/FD sampling, a
  `just capture-acceptance` entry point, and updated ownership, privacy, command,
  qualification, preflight and rollback documentation.
- Validation: `just check` passed: formatting, strict Clippy, 69 Rust tests (one
  opt-in benchmark ignored), no unused dependencies, and RustSec with only the
  already documented unmaintained transitive `paste` warning. Public API fixture
  comparison passed. All 12 backend and 12 collector scenarios passed. The default
  15-check desktop suite passed, and a full 17-check real-Shelllist run passed.
- Three combined desktop attempts timed out while closing Satty via keyboard,
  including the latest combined rerun; one full run passed. Explicit focus did
  not eliminate the intermittent failure. The latest standard 15-check run,
  12 collector checks, 12 backend scenarios and final-package smoke/rollback all
  passed. The combined GUI gate is open, not presented as a stable test guarantee.
- [Detailed qualification](capture-qualification.md) records scope and remaining
  gates: wlr-only/multi-seat, physical login/systemd activation, GUI stability,
  concurrent maximum-size stress/baseline comparison and broader crash injection.
  The implementation is recorded, not a claim that every original release gate
  passed. No production activation, remote push or automatic merge into the
  original working checkouts is part of this stage.
