# Turso engine qualification — stage 1

Release check: 2026-10-29, GitHub `tursodatabase/turso/releases/latest`
reported **v0.8.3**. The crates.io sparse index lists **0.8.3** as the
newest stable public `turso` package (0.9.0 entries are prereleases).
Both committed lockfiles resolve public API, core, SDK and sync internals to
0.8.3. No cloud synchronization is enabled.

Run the isolated Rust probe (never opens desktop state):

```
cargo run --locked --manifest-path tools/turso-contract/Cargo.toml
```

## Verified capabilities

| Capability | Result |
| --- | --- |
| Syscall I/O, FULL synchronous, foreign_keys | Set and read back successfully |
| STRICT types, CHECK, foreign keys/cascade | Invalid rows rejected, cascade verified |
| Prepared parameters, INSERT RETURNING | Reused for 1,000 transactionally inserted rows |
| Native FTS | `CREATE INDEX ... USING fts(body)`, `fts_match`, rollback verified |
| Query planning | Ordered page uses covering `history_order` index |
| Large exact BLOBs | 16 and 64 MiB commits and byte-for-byte reads pass |
| Transactions | Rollback preserves data and search index |
| Recovery | Abrupt subprocess exit after COMMIT and before COMMIT; acknowledged data survives |
| Maintenance | TRUNCATE checkpoint checked, integrity_check returns `ok`, VACUUM reclaims space |

Features: explicitly enable `fts`, not the default global mimalloc allocator;
use `experimental_index_method(true)` for native FTS and
`experimental_vacuum(true)` in the capability probe for controlled maintenance.
Production shutdown uses verified TRUNCATE checkpoint and page reuse, not an
implicit VACUUM. Syscall I/O is the initial backend; no claim that io_uring is
faster has been established.

Initial debug-build measurements on this development machine:

- 16 MiB BLOB commit plus read: 461 ms.
- 64 MiB BLOB commit plus read: 1,884 ms.
- 100 selective FTS queries over 1,000 entries: 41 ms total.
- After retention removes large BLOBs: VACUUM reduces database from
  67,334,144 to 32,768 bytes.

These are synthetic capability measurements, not a desktop latency guarantee.
Production storage uses separate relational metadata, payload and search tables,
indexed recency/favorite/dedup lookups, database-owned policy and artifact links.
Use direct BLOBs: chunking has no demonstrated benefit at the chosen **64 MiB
hard ceiling**, **16 MiB default**. The default retained payload budget is
**256 MiB**, 1,000 regular entries and 100 favorites. Queue count and payload
bytes must be bounded independently. Capture begins closed until privacy intent
and effective policy are verified.

There is no old-store importer, upgrade ladder, backup/export/restore mechanism
or Ringboard fallback. Schema identity mismatches fail closed; they do not
implicitly authorize deleting a file.

Final release reruns, native desktop/package evidence, benchmarks and the scoped
zstd security dependency patch are recorded in [stage 5 qualification](turso-qualification.md).
