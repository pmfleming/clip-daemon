# ADR 0003: daemon-owned regular Wayland capture

Status: accepted design; implementation staged behind the migration gates.
Supersedes the capture-ownership portion of ADR 0002 only after cutover.

## Decision

Implement an event-driven collector in clip-daemon. Keep the existing publication
service and the patched Ringboard storage engine. Primary selection is not captured.
No external `wl-paste` watcher or polling loop will be used.

Use `wayland-client` with ext-data-control preferred, wlr-data-control fallback.
Bind at most one manager, so compositors exposing both cannot duplicate capture.
A dedicated thread owns the event queue and transfer FDs. Tokio owns lifecycle
commands and acknowledged transitions. Missing protocols/Wayland environment are
reported as unavailable while history/API access remains available.

## Compatibility contract

- Capture all supported seats, with one bounded set of offers/transfers shared
  across seats. Seat removal invalidates its offers. Never synchronize primary.
- Reuse the Apache-2.0 SDK's `BestMimeTypeFinder` for representation priority and
  plain-text alias behavior, rather than copying its source. Tests in
  `tests/capture_contract.rs` pin that dependency behavior.
- Inspect all advertised MIME types before requesting data. A password-manager
  marker rejects the whole offer regardless of ordering. Invalid metadata and
  metadata overflow reject the whole offer; truncation must not hide a marker.
- Preserve bytes. Empty/all-ASCII-whitespace representations are skipped and the
  next supported representation is attempted, matching the baseline watcher.
- Initial unpaused process startup may capture the current selection, as before.
  Reattachment after an explicit pause discards bootstrap selections; only later
  selection changes are captured. A Wayland sync barrier defines bootstrap versus
  subsequent events, not a timer or 'drop the next clipboard event' heuristic.
  Reconnect after compositor failure uses the same conservative bootstrap rule.
- Deduplicate on complete payload plus stored MIME. Promotion must preserve the
  main/favorite ring and validate the content proof inside the server reactor.
  Do not reuse an unchecked raw-ID promotion from the legacy watcher.
- Existing self-echo projection and `collapse_self_echoes` behavior stay intact.
  Do not drop all daemon publications: screenshots and standalone publishing need
  capture to create history. Pause does not stop explicit publication or editing.

## Initial resource policy

These are internal constants, not new user-facing settings in clip-api v1:

| Resource | Limit / behavior |
| --- | --- |
| Seats | 16; excess seats unavailable to capture, reported without identifiers |
| Tracked offers | 64 total; overflow rejected, no retained backlog |
| MIME types per offer | 64 |
| MIME bytes per offer | 8 KiB aggregate, each value <= Ringboard's 96-byte limit |
| Metadata budget | 512 KiB maximum for the tracked offer set |
| Concurrent transfers | 4 |
| Payload per transfer | min(configured limit, 64 MiB), plus one overflow-detection byte |
| Aggregate payload reservation | 128 MiB, including queued/in-flight ingest |
| Completed ingest queue | 1; backpressure never blocks lifecycle commands |
| Idle / total transfer timeout | 5 s / 15 s |
| Reconnect backoff | 250 ms to 10 s, cancellable |
| Lifecycle acknowledgement | bounded; failure/uncertain Add never claims verified pause |

Overload drops new offers rather than creating a replay queue. Payloads use
CLOEXEC memory-backed FDs; no filesystem staging before server admission. OS swap
is outside this guarantee. Logging uses fixed reason codes and counters, never
content, offered strings, or content hashes.

## Privacy and mutation barriers

Admission starts closed. Settings and server capabilities must be validated before
opening it. Pause closes admission, cancels transfers, invalidates generations,
and fences submission/acknowledgement before reporting success. Persisted intent
and verified worker state remain distinct. Failure to persist pause is an error;
local collection should still stop where possible. Wipe and retention restarts
use the same fence so pre-transition transfers cannot refill history afterward.

A request already sent to Ringboard can commit before pause completes. A lost
reply cannot be called a verified barrier or retried blindly. Direct Ringboard
clients and unrelated collectors are outside the managed-capture privacy claim.

## Licensing and rollout

The reviewed upstream Wayland/SDK source inherits Apache-2.0; use its public SDK
or preserve notices for any future source reuse. Do not move AGPL-only server
extension code into the MIT daemon. Keep atomic storage operations in the server.

Cutover ships two units and explicitly disables/removes the old watcher. Old and
new collectors are never enabled concurrently. Preserve a known-good three-unit
closure for rollback. No database-format migration or production activation is
part of development. See [the implementation plan](wayland-capture-migration.md)
for staged tests, packaging changes, and manual qualification gates.
