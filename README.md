# clip-daemon

Rust Wayland clipboard capture, embedded **Turso 0.8.3** history, and `clip-api`
for Shelllist. One daemon owns capture, database transactions, privacy policy,
publication, image/file actions and paste targeting. **No Ringboard server, SDK,
policy IPC, fallback backend or Rust bootstrap exception remains.**

This release deliberately starts a new history at
`${XDG_DATA_HOME:-$HOME/.local/share}/clip-daemon/history/history.db`.
Old history and favorites are not imported. Production activation is separate:
read [installation and cutover](docs/installation.md).

## Build and test

Requires Rust 1.95 and the sibling `../daemon-framework` checkout. Use the shared
workspace build tool so local Nix inputs never become persistent revision pins:

```
../daemon-framework/tools/local-build develop .
cargo build --locked
just check
just storage-contract
just backend-regressions
just benchmark-history
just live-acceptance
```

The implementation and new acceptance/benchmark tools are Rust. Shelllist remains
QML; clipboard search, filtering, pagination, retention and policy stay in Rust/SQL.
The native acceptance harness creates a private HOME, bus, database and nested
compositor; it never changes production services or opens normal clipboard history.
`just package-acceptance /nix/store/...-clip-daemon` additionally checks installed
single-service wiring and D-Bus activation.

`just check` includes formatting, strict Clippy, tests, unused dependencies and
RustSec. Advisory refresh requires network access. Native desktop checks require
Hyprland, D-Bus, wl-clipboard and the development shell. A nested test is not a
physical-device power-loss or real-login test.

## Storage and privacy

- One exclusively locked storage-owner thread, 32 queued commands and a 128 MiB
  admitted-input budget. Reads select bounded metadata pages, not full payload catalogs.
- STRICT tables, foreign keys, indexed lookup/order, exact-byte deduplication,
  revision predicates and atomic retention/edit/favorite/delete/wipe transactions.
- FULL synchronous commits; acknowledgements follow COMMIT. Ambiguous commits or
  rollback failures fence storage. Shutdown drains accepted work and verifies a
  WAL checkpoint before releasing ownership.
- Default retention: 1,000 regular entries, 100 favorites, 16 MiB per entry,
  256 MiB total payload. Hard per-entry ceiling: 64 MiB, including capture and
  settings. Byte pressure can evict favorites after regular entries.
- Fresh/missing preferences default to **paused**. Invalid preferences fail closed.
  Explicitly resume capture through Shelllist. Existing valid pause/private intent
  is respected; no native Ringboard settings are adopted.
- Pause/private acknowledgement fences in-flight submissions. Resume skips the
  existing selection, so private-interval content is not replayed. Explicit copy,
  publication and edits remain independent of automatic capture.
- Sensitive publication advertises exclusion markers and never writes history.
  Capture rejects entire sensitive offers, including markers after MIME overflow.

Unknown schemas, foreign files, unsafe paths and corruption fail closed. There is
no automatic destructive reset, schema upgrader, import/export/restore or lossless
rollback path. Newly committed history must still survive ordinary restarts.

## Search and API

Search is a **Unicode-lowercased literal substring**, not fuzzy matching or a query
language. SQL verifies matches against separate full-text content; native FTS
trigrams narrow eligible candidates. Short/punctuation-only queries and oversized
unindexed text fall back to SQL verification, not application-side payload scans.
Ordering is recency-based. Quotes, wildcards and punctuation are literal input.
There is no accent stripping or canonical Unicode normalization.

`clipboard.history.query` accepts `cursor_mode: true` for authenticated,
owner/query/generation/revision-bound cursors (120-second expiry), or offsets for
simple queries. Pages contain at most 200 entries and queries at most 4096 bytes.
Mutations invalidate outstanding cursors. The old `fuzzy` flag and 5,000-row
application-ranking cap are removed; rebuild Shelllist with this daemon.

Useful commands:

```
clip-daemon daemon
clip-daemon store-status
clip-daemon debug protocol-registry
clip-daemon client
```

The checked API registry is `test_support/clip-api-v1.json`. Publication preserves
exact bytes and useful text aliases. Supported actions include copy/paste,
file copy/cut, thumbnails, favorites, bulk delete, inline edits, screenshots,
annotation cancellation, image-as-file, safe URL/file opening and confirmed wipe.
A committed edit with failed external publication is reported as partial success.

The package includes `share/yazi/plugins/yank-to-clip-daemon.yazi/main.lua`.
Its `setup()` handler mirrors Yazi copy/cut selections through
`clipboard.selection.publishFiles` using the packaged daemon's JSONL client.
This integration does not depend on the removed Ringboard backend. Package tests
exercise copy/cut, empty selections and error reporting without touching a live
clipboard.

Generated artifact ownership/references live in SQL. Cleanup checks the private
root, inode, device and owner before unlinking; arbitrary clipboard URI targets
are never cleanup candidates. Old JSON artifact records are not imported.

See [engine evidence](docs/turso-capabilities.md),
[architecture](docs/adr-0004-turso-history.md), and
[qualification](docs/turso-qualification.md). Earlier Ringboard-era documents are
historical records, not current installation or release instructions.
