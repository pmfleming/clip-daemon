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
