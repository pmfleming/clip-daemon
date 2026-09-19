# Plan: move Wayland capture into clip-daemon

Status: proposed; no runtime or deployment changes made by this plan.

## Goal and scope

Replace `ringboard-wayland.service` with a supervised capture worker inside
`clip-daemon`. The final deployment has two services:

- `clip-daemon.service`: Wayland capture, clipboard publication, product policy,
  settings, paste coordination, and the Shelllist API.
- `ringboard-server.service`: persistent history, favorites, retention enforcement,
  pre-write admission, and atomic history mutations.

Keep the database format, opaque entry IDs, and existing `clip-api` v1 methods.
Do not move database writes into the facade, replace Ringboard storage, add X11
capture, collect primary selection, or redesign publication in this migration.
The patched server remains required; this does not make stock Ringboard safe.

Related baseline: [ADR 0002](adr-0002-wayland-selection-ownership.md),
[privacy guarantees](phase4-safety.md), [installation](installation.md), and
[server extension](../packaging/ringboard-policy/README.md). Add a new ADR when
implementation starts; do not rewrite historical decisions as if already shipped.

## Current integration points

| Area | Current implementation | Required change |
| --- | --- | --- |
| Capture | Packaged Ringboard Wayland executable and watcher patch | Daemon-owned event-driven worker |
| Publication | `src/selection.rs`, `wl-clipboard-rs` | Keep working independently of capture |
| Pause/privacy | `src/settings.rs`, `src/settings/services.rs` | Replace watcher systemctl calls with acknowledged worker transitions |
| Retention | Settings restart server, sleep 200 ms, then restart watcher | Quiesce capture, restart server, verify readiness/limits, conditionally resume |
| Startup/shutdown | `src/api.rs`, `src/daemon.rs` | Explicit capture ownership and ordered teardown |
| Storage | `src/ringboard/ipc.rs`, backend transaction lock | Add bounded capture ingest and safe deduplication integration |
| Self echoes | `src/ringboard/artifacts.rs` and history projection | Preserve existing semantics before optimizing |
| Deployment | `packaging/systemd/`, `flake.nix`, `/etc/nixos/home.nix` | Two packaged services; explicit removal of old watcher |
| Tests | `scripts/isolated-acceptance.py`, `backend-regressions.py`, `package-smoke.py` | Exercise in-process capture instead of launching a watcher |

The inspected upstream watcher captures regular selections across seats, ignores
primary selections, selects one representation from each MIME offer, and promotes
existing duplicates. Its MIME chooser rejects password-manager-marked offers and
filters Chromium internal formats. The local patch adds bounded memory-backed
transfers and independent server-side admission.

## Non-negotiable safety properties

1. **One managed capture owner.** Never run old and new watchers together against
   production history. Test comparisons use separate histories and sessions.
2. **No capture before policy.** Load and validate settings before subscribing to
   offers. Missing settings follow documented first-boot defaults; unreadable or
   malformed settings fail closed. Keep the API available to report the error.
3. **Acknowledged pause.** Close admission immediately, cancel/drain outstanding
   transfers, and fence storage submissions before reporting verified pause.
   An Add already sent to the server may commit before the pause acknowledgement;
   no old transfer may be submitted after that acknowledgement. A timeout or lost
   IPC reply is an uncertain transition, not verified privacy.
4. **Bounded, non-disk staging.** Enforce `min(max_entry_bytes, 64 MiB)` while
   receiving, before persistence. Also bound total resident capture bytes,
   concurrent transfers, offer/MIME metadata, queued work, and transfer time.
   Memory-backed files can still be swapped; do not claim otherwise.
5. **Whole-offer exclusion.** Evaluate the complete offer metadata, including
   `x-kde-passwordManagerHint`, before requesting any payload. Never log payloads,
   URLs, clipboard previews, or content digests.
6. **Server remains authoritative.** Every Add still passes server admission before
   eviction/staging. No direct database mutation and no unchecked slot-ID mutation.
7. **Truthful status.** Distinguish desired policy from actual worker state.
   Disconnected/failed is not automatically equivalent to intentionally paused.
   Preserve the existing API's refusal to assert unverified private mode.
8. **Capture and publication are separate.** Pausing capture must not kill an
   already published clipboard selection or disable explicit copy/paste actions.
   Private mode prevents automatic capture, not all explicit history mutations.
9. **No hidden backlog.** Disabled capture retains no offers or payload queue for
   replay on resume. No persistent retry spool when the server is unavailable.
10. **Limited privacy claim.** Control the managed collector only. Direct Ringboard
    clients and unrelated clipboard watchers remain outside this guarantee.

## Proposed internal design

Add `src/capture/` with separate components:

- `controller`: supervised lifecycle and commands (`resume`, `quiesce`, `status`,
  `shutdown`), policy generation, acknowledgements, and bounded control channels.
- `wayland`: long-lived data-control event queue, seat discovery/removal, protocol
  negotiation, offer ownership, and compositor disconnect/reconnect handling.
- `policy`: deterministic MIME selection, exclusions, blank-selection rules, and
  offer/transfer budgets; independently unit-testable.
- `transfer`: cancellable nonblocking reads into private memory-backed FDs,
  idle/total deadlines, incremental bounds, and resource cleanup.
- `sink`: injectable persistence boundary backed by the existing Ringboard backend;
  returns committed/rejected/uncertain outcomes, never publishes a selection.

Use a dedicated supervised Wayland event-loop thread if needed; do not block Tokio
workers or hold the backend transaction mutex across producer I/O. Hand completed
FDs to a bounded ingest worker. Control messages must remain responsive when data
queues are full. Use capture generations plus a serialized submission fence, not
just cancellation flags checked once before an await.

Prefer `ext-data-control`, with a tested `wlr-data-control` fallback where needed.
Select direct Wayland client dependencies during the initial spike: the currently
used `wl-clipboard-rs` paste API exposes one-shot reads, not the long-lived watcher
abstraction needed here. Do not implement polling with repeated `wl-paste` calls.

Proposed lifecycle: `Starting`, `Running`, `Pausing`, `Paused`, `Unavailable`,
`Stopping`. Keep failure reasons and desired intent separate. Shutdown closes
admission, invalidates generations, drains/fences submissions, closes Wayland
resources, and joins workers. Restart backoff must be bounded and cancellable.

Split settings dependencies into `CaptureControl` and engine/service control.
Keep systemd operations for the Ringboard server, not for capture. Construct real
capture only in daemon startup; unit tests and `configure-engine` must not open a
Wayland connection. Headless startup still serves diagnostics/history APIs with
capture marked unavailable.

## Implementation sequence and gates

### 1. Pin behavior and protocol choices

- Record fixtures for MIME preference, aliases/charset handling, cut/copy file
  payloads, image formats, blank/binary content, repeated copies, favorite
  duplicates, multiple seats, source exit, and initial selection at startup.
- Explicitly preserve regular-only capture. No primary-selection synchronization.
- Specify resume semantics: proposed privacy-first behavior discards selection
  snapshots received while reattaching after pause and captures subsequent changes
  only. Test the attachment/event-order boundary so the first real new copy is not
  accidentally discarded. Document any difference from the old watcher.
- Decide initial limits for concurrent transfers, total memory, offer metadata,
  queue depth, and idle/total deadlines; make overload/drop behavior deterministic.
- Verify ext/wlr support against the supported Hyprland build and record fallback
  behavior. Unsupported protocols leave capture unavailable, not silently active.
- Audit licenses before reusing upstream code. The inspected Wayland/SDK code uses
  the upstream Apache-2.0 workspace license; server extensions are AGPL-3.0-only.
  Retain required notices and keep server-only code out of the MIT facade.

**Gate:** written fixtures and ADR decisions; no production behavior changes.

### 2. Introduce lifecycle and policy seams

- Add `CaptureControl`, fake capture worker, state machine, generation/submission
  fence, and fake sink. Temporarily adapt the existing systemd watcher to the new
  interface to keep a runnable baseline during development.
- Refactor `SettingsManager` without changing its public desired/effective contract.
- Define fail-closed handling for settings write failures and worker failures.
  If pause cannot be persisted, keep local admission closed where possible but
  return an error and do not promise pause survives restart.
- Test pause races at receive, queued-ingest, submission, and acknowledgement;
  repeated pause/resume, concurrent settings calls, worker panic, and shutdown.

**Gate:** deterministic unit tests prove no stale-generation submission after a
successful pause; existing API and settings tests remain green.

### 3. Implement capture transport and Ringboard ingest

- Implement long-lived offer tracking and bounded transfer admission, with full
  metadata exclusion before payload reception. Preserve exact chosen payloads;
  normalize text only where baseline fixtures explicitly require it.
- Introduce an internal bounded Add path through Ringboard IPC/SDK. Require the
  policy-engine handshake/readiness gate, rewind admitted FDs, reject the reserved
  `u64::MAX` Add result, and invalidate backend identity/search state after success.
- Do not retry an Add blindly after an ambiguous reply; report/reconcile uncertainty
  before further ingestion. Exactly-once ingestion across crashes is not promised.
- Preserve deduplication semantics using full-content plus stored-MIME identity and
  revalidation of candidates. Audit the SDK deduplicator before adopting it.
  If safe promotion is absent from the policy protocol, add a negotiated,
  proof-checked promote operation that preserves the ring/favorite state rather
  than using the legacy unchecked MoveToFront call. Qualify full rings and slot reuse.
- Quiesce ingestion during wipe and invalidate pre-wipe offers so they cannot refill
  history afterward. Bound/no-spool handling applies while the server is down.

**Gate:** fake-producer transport tests plus disposable real-server Add,
admission, duplicate/favorite, slot-reuse, wipe, and uncertainty tests.

### 4. Integrate policy, publication, and failure recovery

- Start capture only after validated settings and successful engine capability/
  limit checks. Preserve API access if either capture or engine is unavailable.
- Replace the retention restart sequence: quiesce, persist/apply configuration,
  restart engine, wait for actual readiness and negotiated limits (not a fixed
  sleep), then resume only if current desired policy permits it. A failed sequence
  leaves capture stopped/unavailable and reports saved-but-unapplied settings.
- Lowering a byte limit cancels oversized in-flight work; raising it does not
  retroactively enlarge a transfer's budget. Revalidate at ingest as well.
- Keep the current publisher and artifact/projection echo rules first. Do not
  blanket-drop every daemon-owned selection: screenshots and standalone `publish`
  create history through recapture today. Edits, restores, generated URIs, and
  `collapse_self_echoes=false` each need explicit regression coverage.
- Do not trust a public MIME marker as proof of daemon origin. Defer stronger
  publication/capture correlation until its semantics are separately tested.
- On compositor disconnect, cancel offers/transfers and reconnect with backoff,
  rechecking policy and discarding stale generations before admission opens.
- Expose metadata-only health/drop reasons. Keep existing capture fields compatible;
  add optional diagnostics only with updated protocol/consumer contract fixtures.

**Gate:** nested-compositor tests demonstrate capture, pause/resume, private startup,
publication, reconnection, and retention failure recovery with no external watcher.

### 5. Cut over packaging and user configuration

- Make in-process capture the packaged daemon default only after prior gates pass.
- Remove watcher dependencies from `clip-daemon.service` and
  `ringboard-server.service`. Keep server `Type=notify`, `configure-engine`, private
  permissions, and startup ordering. Avoid dependencies that stop the facade during
  its own requested server restart and prevent it reporting the outcome.
- Stop shipping/enabling `ringboard-wayland.service`; adjust `flake.nix`
  substitutions, package smoke expectations, and qualification tools accordingly.
  Retire `capture-allowed` only after no packaged consumer uses it.
- In `/etc/nixos/home.nix`, remove the packaged watcher mapping while retaining the
  daemon/server mappings and graphical-session overrides. Removing a unit from a
  package alone does not stop an already-running old watcher.
- Provide an explicit migration preflight: back up history, review retention and
  saved privacy intent, stop/disable the old watcher, apply the new units, reload,
  start the daemon, verify two-service health and one capture owner. Inspect
  standalone/autostart collectors as well as systemd units. Do not silently switch
  to a second collector if startup fails.
- Keep the old watcher executable/patch available for a rollback qualification
  window if useful, but never launch it alongside in-process capture. Remove
  watcher-specific patch/build code only after that window; retain server admission
  and policy mutation code permanently.

**Gate:** disposable package activation/upgrade/rollback tests; manual login and
session-restart validation; no unattended production migration or retention change.

### 6. Finish qualification and documentation

- Update the existing acceptance harness to launch only server and daemon. Replace
  assertions against watcher log strings with bounded state/history assertions.
- Update fake-systemctl privacy tests to inject capture failures; retain engine
  restart failure tests. Keep stock-server rejection coverage.
- Update README, installation, safety guarantees, qualification results, command
  help, service descriptions, and the architecture ADR to match the new ownership.
- Run Rust formatting, Clippy, all tests, contract checks, backend regressions,
  isolated desktop acceptance, package smoke/upgrade tests, and hardware gates.
- Record idle CPU, capture latency, peak RSS/FDs under concurrent maximum-sized
  offers, and resource recovery after cancellation/reconnect against the baseline.

**Gate:** recorded results, with any untested compositor/protocol combination
explicitly unsupported or pending rather than reported as passing.

## Required acceptance matrix

- Plain UTF-8/aliases, non-UTF-8 text, HTML/JSON, images, URI lists, GNOME copy/cut,
  unknown binary types, empty/blank offers, and malformed/oversized MIME metadata.
- Sensitive marker in any position in the offer list: no payload read or history
  change; Chromium internal formats never become unintended captured content.
- Limit minus one, exact limit, limit plus one, infinite/slow producer, transfer
  timeout, burst overload, and policy reduction during transfer: bounded resources
  and no disk staging/eviction for rejected capture.
- Source exits after providing data; independent clipboard publication still works
  while capture is paused; existing GTK/terminal/Shelllist paste paths remain valid.
- Pause during each pipeline stage, malformed settings at login, persistent private
  mode across daemon restart, resume without private backlog, and worker failure.
- Daemon publications: restore, inline edit, annotation, screenshot, Yazi files,
  standalone publish, and both self-echo collapse settings.
- Duplicate promotion in full main/favorite rings, concurrent external mutations,
  deleted/reused candidate IDs, server restart, lost Add reply, and wipe fencing.
- Multiple seats and seat removal, ignored primary selection, compositor restart,
  missing protocols, D-Bus activation without a compositor environment, logout,
  and bounded shutdown with a blocked producer/server.
- Upgrade from three services to two and rollback: preserved settings/history,
  correct unit dependencies, no overlapping managed collectors or privacy gap.

## Rollback

Keep the prior known-good daemon/server/watcher package closure and declarative
configuration. Stop the new daemon (which stops its collector) before restoring
old units; retain paused/private intent throughout. Restore matching packaged
services, verify the old watcher's privacy startup condition, then start only the
selected stack. No database schema migration is planned. A rollback does not undo
retention eviction, hence the pre-cutover backup and settings review.

## Definition of done

Only daemon and server are enabled; the daemon captures regular Wayland selections
without the Ringboard watcher. Privacy transitions are verified pipeline barriers,
not service-state guesses. Existing edits, capture, publication, deduplication,
and paste behavior pass the recorded tests. The server still enforces storage
safety independently, and deployment mismatch is surfaced in diagnostics before
capture begins.
