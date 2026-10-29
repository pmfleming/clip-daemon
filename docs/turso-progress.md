# Turso replacement: completed

All five stages are implemented and committed separately. Production cutover is
intentionally not performed; the tree is ready for the operator's rebuild.

1. **Capability gate — `3bbd47c`.** Pin public Turso 0.8.3 without wrapper default
   features; Rust probe qualifies real SQL/constraints/prepared statements/native
   FTS, exact 16/64 MiB BLOBs, abrupt-exit WAL recovery, checkpoint and VACUUM.
2. **Bounded relational store — `ee4486d`.** Exclusively locked worker, bounded
   queue/bytes, normalized STRICT schema, transactional revisions/dedup/retention,
   native literal-search candidates, exact payloads, safe path/schema refusal,
   durable artifact ownership/references and latched failure/shutdown behavior.
3. **End-to-end replacement — `b74765f`.** Turso-only backend and direct policy,
   admission/privacy/shutdown barriers, SQL pages and authenticated cursors, native
   actions/editors/artifacts. Delete SDK and in-memory ranking/projection. Matching
   Shelllist caller/defaults: `68b6894` in its repository.
4. **Single-service packaging — `4a346a9`.** One daemon/activation target; delete
   legacy packages, patch, units, obsolete acceptance adapters and Yazi plugin.
   Deployment stops/masks retired collectors rather than pulling them back in.
   Companion NixOS configuration: `6696ec8` in `nixos-capture`.
5. **Qualification and release hardening — this commit.** Real storage/fault and
   desktop/package checks, 1,000/10,000-entry benchmarks, real targeted QML paste,
   Satty save/cancellation and grim screenshot, installed Nix private activation,
   native Quickshell socket/large-frame tests and QML clipboard regressions
   (companion native fixture: Shelllist `62638bd`).
   Fix raw/semantic MIME separation, echo-current projection, no-op recapture,
   bounded multi-trigram search, metadata-only revision reads, isolated API-only
   defaults, strict native-test DTOs, Crane path-patch builds and source filesets.
   Update Tokio and scoped zstd dependency to resolve audit vulnerabilities.

See `turso-qualification.md` for exact evidence, performance budgets and limits;
`src/history_store/README.md` for invariants; `installation.md` for deliberate
fresh-store deployment. The old history/favorites are neither read nor imported.
There is no legacy production adapter or fallback. Four upstream audit warnings
remain documented; real login and physical power-loss checks are not claimed.
