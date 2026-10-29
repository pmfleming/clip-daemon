# ADR 0004 — daemon-owned relational Turso history

Status: implemented in the staged Turso rewrite; supersedes the storage portions
of ADRs 0001–0003. Capture/publication remain separate lifecycles.

## Decision

Use embedded local-only Turso 0.8.3, with the public Rust crate, core and SDK
locked to the same release. Enable native FTS explicitly, use syscall I/O, FULL
synchronous transactions and foreign keys. Native index methods and maintenance
are qualified by the isolated Rust probe. Do not keep a Ringboard abstraction,
server, SDK, policy protocol, native configuration reader or fallback store.

The new store is intentionally empty. No importer, old-data backup/export,
restore, schema upgrade, reverse migration or legacy identity mapping exists.
Application/schema identifiers detect incompatible or foreign files; errors do
not implicitly authorize deletion.

## Ownership and database model

A dedicated thread exclusively owns the connection and file lock. A bounded
queue (32 commands) and byte reservations (128 MiB queued/executing input) reject
excess work before copying input payloads. Wayland dispatch never performs SQL.
Only after COMMIT may a write be acknowledged; accepted work is not cancelled
merely because its waiter goes away. Ambiguous commit/failed rollback fences
subsequent unsafe work. Shutdown closes submissions, drains jobs and verifies
checkpoint completion before releasing ownership.

`store_meta` owns identity, catalog revision and recency sequence.
`effective_policy` is authoritative admission/count/byte policy.
`entries` holds UUID identity, revision, recency, favorite, MIME/digest/length,
classification and preview. `payloads` separates exact BLOBs from hot metadata.
`search_content` holds searchable text and the native FTS representation.
`artifacts` and `entry_artifacts` own generated file records and references.
Foreign keys, unique keys, STRICT types and CHECK constraints enforce integrity.

Prepared parameterized SQL implements page filtering/count/order, exact-content
candidate lookup, all-or-nothing bulk mutations and set-based retention. There is
no full-history Rust projection, raw ring-slot map or application-side ranking
cache. Current-row and page metadata are selected within one owner-serialized
snapshot. Cursor authentication stays in Rust; SQL remains the query engine.

Deduplication checks SHA-256, MIME and size, then **exact BLOB equality** before
promotion. Replacement retains identity and recency position but advances its
revision. New captures promote matching records. Stale expected revisions reject
mutations; bulk deletion validates every target before deleting anything.

## Search

A literal Unicode lowercase substring contract retains useful punctuation/file
path search without exposing FTS query syntax. Native n-gram FTS narrows eligible
candidates, then SQL `instr` verifies exact matches. Query input is always bound,
not interpolated. Oversized text has bounded index amplification and falls back
to SQL verification; short or punctuation-only searches may scan searchable text.
Raw payloads are not read into a Rust full-history matcher. Results follow recency,
not fuzzy relevance. Cursor pages invalidate on catalog changes and expire after
120 seconds. There is no previous 5,000-row search cap.

## Policy and external effects

Admission starts closed on every open. Fresh preferences default paused; malformed
intent blocks activation. Policy updates quiesce capture, commit direct SQL
retention and only then acknowledge the effective state. Controller pause and
shutdown fence accepted submissions. Reconnect/resume skips the bootstrap selection
so private-interval data is not replayed. Sensitive offers never reach storage.

Wayland publication is external to SQL: an edit can commit while publication
fails, and the response reports that partial outcome. Editor/screenshot operations
retain cancellation and wipe fences. Filesystem writes likewise cannot be part
of an SQL transaction: privately stage files, record ownership/references, and
reconcile conservatively. Cleanup requires the recorded device/inode/owner and
expected private root; it never deletes an arbitrary URI target. Old artifact
registries are ignored rather than converted.

## Deployment

One packaged daemon service and one D-Bus activation file. Both retired Ringboard
units are conflicted/masked at deployment, not invoked. No startup importer,
backup operation or profile switch runs as a side effect of building/testing.
The explicit operator cutover deploys the matching QML cursor caller and starts
new history. See installation and qualification documents for evidence and limits.
