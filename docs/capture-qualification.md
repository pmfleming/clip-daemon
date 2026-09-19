# Daemon capture qualification

## Scope

The migration branch implements the two-service package; production has **not**
been activated. Tests use disposable HOME/XDG directories, session buses, history
and nested compositors. No production clipboard content is test input.

Qualified desktop: Hyprland 0.55.4, ext-data-control, policy-enabled Ringboard
0.16.2, GTK3, Ghostty, Satty, and Shelllist 0.2.0 / Quickshell 0.3.0. The collector
prefers ext when both protocols are advertised. Merely advertising wlr does not
qualify the wlr-only path.

## Recorded automated evidence

| Check | Result / scope |
| --- | --- |
| Formatting, strict Clippy, locked all-target/all-feature tests | 69 passed, one opt-in benchmark ignored; includes capture fences, resource budgets, malformed MIME metadata, transfer deadlines, API/settings, and screenshot work integrated from main |
| Dependency and public API checks | No unused dependencies; RustSec reports only the known unmaintained transitive `paste` warning; generated public API fixture matches the checked-in contract |
| MIME compatibility fixture | Passed, 14 SDK cases including aliases, image/file priorities and sensitive marker ordering |
| Backend regressions | 12 passed: admission, capture proof mismatch/oversize, ring-preserving promotion, MIME distinction, slot reuse, concurrent mutation, search and artifact identity |
| Integrated collector | 12 passed: capture/dedup, edits in main/favorites without duplicate echoes, primary exclusion, pause/no replay, sensitive exclusion, exact/oversize admission, private restart, corrupt settings, retention recovery, wipe/no refill, compositor reconnect, 64 MiB hard limit |
| Standard nested desktop | 15 passed, including source exit, GTK/Ghostty paste, images, Satty save/cancel, multi-file cut/copy, sensitive/oversized rejection and deletion/wipe |
| Actual Shelllist layer-shell | Both additional GTK and Ghostty paste checks passed; one combined 17-check run passed, but the latest combined run timed out at Satty (see below) |
| Installed package | Two units, legacy conflict/order, private files, invalid-policy rejection, installed D-Bus activation, private startup and live v2 negotiation passed |
| Synthetic upgrade/rollback | Previous engine -> new engine preserves synthetic history/private intent; no schema conversion or production data involved |
| Declarative configuration | Two enabled packaged units, no duplicate declarations, no watcher enablement, retired unit resolving to `/dev/null`; Nix formatting/deadnix/statix passed |

Three combined desktop attempts timed out while closing Satty via keyboard.
One full 17-check run succeeded, but the latest combined rerun timed out again.
The latest standard 15-check run passed. The harness explicitly focuses and
observes the editor before sending keys; that did not eliminate the combined-run
failure. GUI timing stability remains an open qualification gate, not a proven
production capture failure or a universally reliable test.

### Regression discovered during qualification

Publishing an inline text edit initially created a duplicate: replacement stored
`text/plain` while Ringboard Add/capture stored its canonical empty plain-text MIME.
The live test failed before the fix. Replacement and capture now share that
normalization; main and favorite edits pass without an extra echo entry. Other
MIME distinctions remain intact and server content-proof validation is unchanged.

Seat teardown now releases supported seat proxies, losing the last seat reports
unavailability, and the offer cap includes active transfers as well as queued
offers. Multi-seat compositor behavior still needs dedicated qualification.

## Resource observations, not a comparative benchmark

`capture-acceptance.py` records metadata-only `/proc` samples in
`target/capture-acceptance/metrics.json`. One debug-build run recorded:

- 0.01 CPU seconds over a one-second idle sample (coarse scheduler accounting).
- 15.7 ms from synthetic copy invocation to query-visible history, including CLI
  and D-Bus overhead; not a latency distribution or service-level guarantee.
- 23,048 KiB initial RSS and 23,760 KiB RSS after a real 64 MiB capture.
- 11 daemon FDs while capturing, falling to 10 after acknowledged pause.
- A 64 MiB + 1 byte offer rejected without changing history; exactly 64 MiB accepted.

Samples span daemon restarts; CPU totals are per process. RSS/high-water values are
kernel samples, not an accounting of memory-backed FDs, compositor or engine
memory. Aggregate payload reservations are separately bounded at 128 MiB in code
and tested. These observations do **not** establish concurrent worst-case memory,
long-run leak freedom, or improvement over the previous watcher.

## Repeat

Inside the current-source development shell:

```sh
python3 ../daemon-framework/tools/local-build.py develop .
just check
just backend-regressions
just capture-acceptance
just live-acceptance
# Optional real Shelllist layer-shell checks:
SHELLLIST_QML_ROOT=/path/to/shelllist-config/share/shelllist \
SHELLLIST_SEARCH=/path/to/shelllist-search/bin/shelllist-search just live-acceptance
```

Build via the same helper and run `scripts/package-smoke.py PACKAGE [PREVIOUS_PACKAGE]`
for installed-artifact and synthetic rollback checks. Desktop logs/results are in
`target/live-acceptance`; collector logs/metrics are in `target/capture-acceptance`.
Logs contain synthetic fixtures only and stay local. See
[installation](installation.md) for cutover and backup requirements.

## Remaining release gates

Do not infer these from the passing automated checks:

- Real wlr-only compositor and multi-seat/hotplug qualification.
- Physical keyboard, login/logout, real user-systemd activation and session restart.
  Package smoke uses a disposable D-Bus daemon, not a systemd VM/login session.
- Repeated stable GUI runs, including the intermittent Satty timeout above.
- Concurrent maximum-size offer stress, malicious/slow producer soak, and a
  controlled before/after CPU/latency/peak-resource comparison with the old watcher.
- Full process-level failure injection at every receive/queue/commit boundary,
  including lost storage acknowledgements. Deterministic unit fences and backend
  tests are evidence for their scope, not substitutes for every crash scenario.
- Production backup, review of retention/private intent, explicit old-watcher
  shutdown, and deliberate activation. Development does not perform these actions.

Implementation and local automated qualification are recorded; the full deployment
acceptance matrix remains open until these gates have actual results.
