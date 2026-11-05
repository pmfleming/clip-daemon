# Relational history store

`HistoryStore` synchronously submits owned requests to one storage-owner thread. The
queue holds at most 32 requests; reservations bound queued and executing input to
128 MiB, acquired before payload copies. Excess admission fails instead of waiting
with another unaccounted copy. Shutdown closes admission to the queue, drains
accepted requests, closes persisted capture admission, verifies a TRUNCATE
checkpoint, and releases ownership. Terminal shutdown failures remain latched.

The worker uses Turso 0.8.3, syscall I/O, FULL synchronous durability, foreign keys,
and native index methods. The database and sidecars live inside a private owned
directory. The nofollow, single-link owner lock remains held until the connection
closes. Existing empty files without recovery WAL, unknown identities, modified
schemas, and corrupt databases fail closed. No repair, reset, migration, or legacy
reader is provided. Valid newly committed WAL is recovered on restart; capture
admission always starts closed.

## Transactions and retention

Entries have immutable UUID identities and catalog-derived monotonically increasing
revisions. Exact raw MIME and bytes are durable; summaries independently retain
resolved MIME and content classification. An indexed digest/MIME/size candidate
lookup additionally compares BLOB bytes before promoting a duplicate. Promotion
changes revision and recency; replacement preserves ID and position. Observing
already-current exact content is a no-op, including our own publication echo.
Favorite changes invalidate the previous revision. No-op favorite writes do not.

Mutations and policy changes commit retention and the catalog revision together.
Bulk deletion validates all targets with a relational VALUES set before one DELETE.
Category retention keeps the newest main/favorite entries independently. Global
byte retention prefers favorites, then recent main entries. A mutation's target is
protected from eviction; a result that cannot be retained causes rollback. Policy
reductions may evict favorites. Empty-category limits are supported. Defaults are
1,000 main entries, 100 favorites, 16 MiB per entry, and 256 MiB payload total; the
supported entry ceiling is 64 MiB. Total limits count raw payload bytes, not physical
index/WAL overhead. A failed COMMIT or ROLLBACK fences subsequent work until restart.

## Search

Search is **Unicode lowercase literal substring**, not fuzzy matching, stemming,
regex, query syntax, or Unicode full case folding. For example, `Straße` matches
`straße` but not `STRASSE`. Searchable content comprises raw MIME, a newline, and the
complete payload when valid UTF-8. Invalid UTF-8 payloads remain restorable but only
their MIME is searchable. Results put favorites first, then ordinary history,
retaining recency order within each group before applying offsets and limits.

A native FTS trigram index narrows candidates by intersecting up to 32 generated,
quoted alphanumeric trigrams from the lowercased query. Final SQL `instr` verification
preserves exact substring semantics, including punctuation and arbitrary literals.
The FTS index covers complete searchable documents no larger than 64 KiB. Larger
documents are explicitly marked unindexed and always included for SQL verification,
preventing false negatives while bounding native-index amplification for 64 MiB
entries. Short/punctuation-only searches use SQL verification directly. Empty-query
browsing touches metadata only. The worker never scans
history payloads in Rust or retains a full-history cache. Page, total, current entry,
and revision are read under the same serialized owner snapshot.

## Generated artifacts

SQL ownership records persist path, source entry, image identity, creation time,
device, inode, and owner. All URI lines in bounded payloads are inspected at mutation
time and references persist relationally. Cleanup candidates exclude both live
source entries and URI references. Wipe deletes references but deliberately retains
ownership records so the backend can safely reconcile files. Only the backend
performs unlink after root/device/inode/owner checks; SQL ownership is not permission
to delete an arbitrary user URI. Verified generated-image echoes may be hidden by a
query; source deletion or replacement removes their links.

## Tests

`cargo test --locked --test turso_storage` exercises real on-disk stores, including
64 MiB exact restoration, concurrency, stale revisions, atomic bulk mutation,
retention, native indexed Unicode substring search, artifacts, private paths,
schema rejection, normal restart, and abrupt subprocess-exit WAL recovery.
`cargo test --locked --lib history_store::` additionally tests deterministic queue
and byte bounds, draining, latched shutdown failures, real-engine constraints and
query plans, index rollback, and ambiguous commit/failed rollback fencing.
