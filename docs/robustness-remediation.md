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
