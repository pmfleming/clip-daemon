# Single-service package and isolated activation contract

The production package installs exactly one user unit (`clip-daemon.service`)
and one D-Bus service (`org.laufan.ClipDaemon.service`). Both select the same
absolute packaged executable. The unit uses Type=dbus, private umask and a
30-second stop timeout; retired Ringboard services are conflicts, not runtime
dependencies. There is no SDK/bootstrap requirement or second storage process.

The Rust `examples/turso_acceptance.rs --package OUT --nested` harness checks the
installed paths and activates the actual installed daemon on a **private** bus.
Its isolated HOME/XDG directories contain only synthetic clipboard data. The bus
configuration does not import host activation service directories. The runner
verifies daemon ownership and private environment before exercising mutations.
Every long-lived child has a separately owned process group retained until cleanup.
No production systemd manager, old history store or normal clipboard is used.

`tests/turso_acceptance.rs` checks package source wiring and real fresh-store
contracts. `tests/turso_storage.rs` exercises SQL storage invariants;
`tests/turso_faults.rs` exercises subprocess failure/recovery. The old Python
smoke service and Ringboard rollback/upgrade acceptance fixtures were deleted.

This is not a real login or hardware power-loss test. The production operator
must deliberately stop retired services, deploy matching daemon/QML units and
verify the resulting managed topology as described in `docs/installation.md`.
