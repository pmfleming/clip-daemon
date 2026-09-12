# Robustness remediation

Each step is committed separately. Regression tests use disposable HOME/XDG,
Ringboard sockets and D-Bus; they never mutate the user's clipboard history.

## 1 — ring storage reuse

Removed per-slot summary reuse. Cached projections are scoped to a history token;
changed history is fully re-inspected. Action-time identity validation always
hashes the current entry, without consulting the projection. Unreadable entries
are retried, and a history change during projection construction rejects the
snapshot instead of installing a mixed-generation projection.

Validation: Rust tests and strict all-feature Clippy pass. `python3
scripts/backend-regressions.py wraparound` exercises main and favorite ring
wraparound, stale deletion without an intervening query, and details/preview
agreement against real Ringboard 0.16.2.

## 2 — eviction-safe replacement

Replaced add/swap/remove with a negotiated server-side compare-and-replace
extension. The patched `.#ringboard` server stages outside both retention rings,
checks the entire content/MIME proof inside its request reactor, and retains the
original slot without advancing either ring. Stock servers fail closed before
any mutation. The extension and AGPL server patch are documented under
`packaging/ringboard-policy/`.

Validation: full main/favorite rings retain every row when editing their oldest
and newest entries. The original ring order is preserved. A stock-server test
verifies rejection leaves the original untouched. Rust tests and strict Clippy
pass. No production Ringboard service was replaced or restarted.

## 3 — complete generated-file references

Ownership references are collected independently of echo matching and previews.
Every URI line is inspected, including those after the preview prefix and the
100-file details limit. Multiple references are retained. Oversized/uninspectable
lines conservatively retain registered files instead of authorizing deletion.

Validation: unit coverage and a real-backend regression retain two referenced
files beyond 100 long URI lines while deleting only a third unreferenced file.
Rust tests and strict Clippy pass.

## 4 — verified privacy transitions

Saved intent and effective capture state are separate. API legacy pause/private
booleans are never asserted when service state is unknown; `settings.get` also
returns desired state, verification and errors. Every pause/resume request
retries idempotent service control and verifies ActiveState. Startup reconciles
saved intent; settings reads detect external capture restarts. Service commands
have deadlines and kill-on-drop. A failure preserves intent for a later retry,
not an unverified privacy claim.

Validation: deterministic injected failures plus isolated D-Bus tests cover two
failed attempts, successful retry, daemon restart and an external service-state
change. Rust tests and strict Clippy pass.

## 5 — cancellation/commit boundary and cleanup barrier

Annotation control has mutually exclusive editing, committing and cancelled
states. Cancellation can win only before the blocking commit begins; once it
begins, the operation keeps its files and reports the actual completion/failure.
Cleanup and wipe block new annotation launches, cancel editors with terminal
events, and wait for committing jobs before touching history or caches. Backend
jobs are serialized against daemon-local queries/mutations to prevent cleanup
and identity installation from interleaving.

Validation: a deterministic real `spawn_blocking` test holds a commit at a
barrier, verifies cancellation is refused and files remain, then verifies cleanup
waits and receives exactly one completed event. Pre-start/running-editor
cancellation tests still pass. Rust tests and strict Clippy pass.

## 6 — per-subscriber history baselines

Receivers are attached before the subscribed acknowledgement, and each history/
current subscriber receives a freshly sampled initial reset (or unavailable
state). This is independent of the shared poller's baseline and closes the
query-to-subscribe race for additional frontends.

Validation: two simultaneously connected JSONL clients each receive initial
history and current resets against real Ringboard. Rust tests and Clippy pass.

## 7 — full-content annotation echo identity

Echo matching reuses the complete streamed content digest, not its preview.
Persisted echo records carry identity version 2; weak legacy records are discarded
rather than guessed or migrated from incomplete data. Generated-file ownership
records remain intact.

Validation: differing image suffixes and truncated prefixes do not match in unit
tests. Two valid PNGs with identical first 64 KiB and different final pixels both
remain visible in the real-backend regression. Rust tests and Clippy pass.

## 8 — exact PNG editor contract

Annotation/screenshot output must be a non-symlink regular file whose detected
format is PNG and whose full decode passes the existing limits. A filename
ending in `.png` is not treated as format validation.

Validation: PNG succeeds; JPEG/GIF/TIFF disguised as PNG, symlinks and truncated
PNG fail. A real adapter returning GIF to its PNG output path emits failure and
leaves history unchanged. Rust tests and strict Clippy pass.

## Gap 9 — real-backend concurrent mutations

Extended server-side proof validation to delete and favorite changes. Bulk delete
validates every unique proof before removing anything, and wipe runs in one
reactor turn. All facade history mutations now require the negotiated package;
there is no unsafe fallback to stock multi-request IPC. Capture/other clients
cannot interleave these operations. This is concurrency atomicity, not a promise
that a disk failure or power loss can never produce a partial outcome.

Validation: eight independent sockets race replacements against the same proof:
exactly one commits and seven receive stale status. A mixed valid/stale bulk
selection deletes nothing. Stale removal, favorite and wipe paths pass against
the real patched server. Full-ring regressions, Rust tests and Clippy pass.

## Gap 10 — initial and effective retention configuration

On first use, existing native Ringboard count limits are adopted instead of
silently replacing a user's retention policy. Fresh installations use the
advertised defaults. `configure-engine` validates and persists both configurations
before server startup; daemon startup reconciles saved intent. Persisted JSON is
range-validated, not merely deserialized.

The server exposes its actual active limits through a read-only policy request.
`settings.get` reports desired/effective retention and synchronization separately.
Even a no-op update checks effective state, so a failed restart is retried rather
than reported as applied. Missing byte enforcement is explicitly reported as
`max_entry_bytes: null` until the capture-policy step.

Validation: real-server tests cover native limit adoption, two failed restart
attempts with desired/effective mismatch, subsequent server restart/recovery, and
rejection of invalid persisted limits. Rust tests and strict Clippy pass.

## Gap 11 — complete text search

Nonempty searches inspect complete textual entries using a streaming Unicode
lowercase matcher with bounded buffers, including matches spanning read/UTF-8
boundaries. Previews and MIME matching remain available. Queries are limited to
4096 bytes; one search-result set is cached per history token for pagination.
No full clipboard-text index is persisted or retained in memory. Inline image
and unknown binary payloads are not decoded as searchable text.

Validation: a 100+ KiB real entry matches beyond the preview, across a multibyte
boundary and across a newline. Repeated queries agree, eviction invalidates cached
hits, oversized queries fail validation, and malformed UTF-8 fails safely. Rust
tests and strict Clippy pass. Historical projection-only benchmark results do not
measure this new full-text scan.

## Gap 12 — capture-side admission and sensitive-marker qualification

The patched watcher stages all offers in bounded memfds, not disk scratch files,
and discards oversized offers before sending them to Ringboard. Both watcher and
server use the same atomically written byte-policy file; malformed policy fails
closed. The server independently snapshots/admit-checks every Add before freeing
a retention slot or writing disk data, and replacements obey the configured
limit too. The effective ceiling is min(configured bytes, 64 MiB). Legacy Add
rejections return an impossible ID; the packaged CLI/watcher handle it explicitly.

Validation: oversized Add into a full ring, malformed policy, edit-size enforcement
and recovery preserve history in backend regressions. **All 13** nested-desktop
checks pass, including a multi-MIME password-manager-hinted offer rejected before
capture and an oversized binary offer rejected before persistence, followed by
successful normal capture. The producer, payloads and desktop are synthetic.

Limits: memory-backed buffers can be swapped by the OS; this is not a no-swap
security guarantee. Unmarked password fields cannot be identified through
Wayland data control. Existing stored entries are not retroactively erased.

## Gap 13 — reproducible packaging and privacy-safe service startup

Pinned the framework to a public Git revision (Nix builds no longer require a
local sibling), corrected patched Ringboard vendoring, and built both packages
in the Nix sandbox. The daemon ships absolute-path server/watcher units with
engine configuration before readiness and a fail-closed persisted-intent
ExecCondition before capture. Dependencies, installation, conflict migration and
upgrade steps are documented in `docs/installation.md`.

Validation: packaged Ringboard passes all 11 policy-backend regressions. The
sandboxed daemon build passes 38 unit tests plus seven integration tests. A
clean-HOME package smoke test verifies installed unit syntax, private first-boot
configuration, paused/malformed startup rejection, installed D-Bus activation and
synchronized live limits. No production units were started. Full login/systemd
VM qualification remains distinct from this smoke test.

## Gap 14 — real Shelllist layer-shell and terminal paste acceptance

Added real Ghostty and actual packaged Shelllist/Quickshell clients to the
isolated desktop harness. It exercises the real picker/controller, Enter action,
hide animation/session handshake, GTK Ctrl+V and Ghostty Ctrl+Shift+V. With no
remaining target windows, paste safely degrades to retained clipboard content
for manual paste. All non-clipboard Shelllist daemon executables are stubbed.

Validation: **all 17 checks pass**, including both actual layer-shell target
types. Without explicit Shelllist inputs, its two checks are visibly NOT RUN;
the default 15 checks still include the real terminal and missing-target case.
Store/client inputs are recorded with results. Physical keyboard, login/session
and production-service qualification remain manual; no such records were forged
or automatically marked passed.
