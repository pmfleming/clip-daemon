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
