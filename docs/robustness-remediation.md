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
