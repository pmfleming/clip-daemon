# Ringboard policy extension v1

The Nix `ringboard` output applies this narrowly scoped patch to the pinned
Ringboard 0.16.2 source. The added server code is **AGPL-3.0-only**, matching
Ringboard's server license. The daemon remains MIT licensed and communicates
over a socket. No database files are written by the daemon.

Legacy Ringboard clients retain protocol 0. Policy clients negotiate byte `0xc1`
instead. An unpatched server returns version 0 and closes the connection; the
daemon refuses to send any mutating request in that case. Never send extension
packets to a server without a successful policy handshake.

Packets: `CDP1`, operation byte (1 = compare-and-replace, 2 = compare-and-remove,
3/4 = compare-and-favorite/unfavorite, 5 = wipe, 6 = validated bulk delete,
7 = effective limits), raw entry ID (u64 LE),
32-byte expected content proof, MIME length (u8, maximum 96), MIME bytes. Replace
also passes one regular file descriptor using SCM_RIGHTS. Bulk delete passes a
regular file containing u32 LE count followed by that many (u64 LE ID, 32-byte
proof) pairs; 1–5000 unique targets are validated before any removal. Wipe and
bulk delete execute in a single reactor turn, so other requests cannot interleave.
Disk-I/O failures are still reported as potentially partial outcomes. Replies are `CDR1`
plus status: 0 committed, 1 stale/missing, 2 rejected/error. IPC waits are bounded.
A lost reply is an uncertain outcome: refresh history before retrying.

Operation 7 returns `CDS1`, main capacity (u32 LE), favorite capacity (u32 LE),
and enforced capture byte limit (u64 LE; zero explicitly means unsupported).
These are runtime values, not a reread of desired configuration.

The proof is SHA-256(`clip-daemon:proof:v1:` || content_digest || stored_mime).
Content digest is SHA-256(`clip-daemon:entry-content:v1:` || all stored bytes).
Empty stored MIME stays empty in the proof. This detects storage reuse and MIME
changes without depending on preview bytes or a lossy JSON integer.

Replacement validates the proof in the server's single-threaded request reactor,
stages a complete file **outside both rings**, and installs it at the original
slot. It never advances the write head or adds a retained entry. Other capture
and mutation requests cannot interleave the comparison and replacement. Both
bucketed and file-backed entries are supported. MIME-xattr support is required;
unsupported filesystems fail closed without changing history.

This closes full-ring eviction and concurrent-IPC replacement races, not a claim
of database-wide power-loss ACID transactions. Ringboard's existing durability
and recovery model still applies. Staging/sync and install errors are reported;
failed old-file cleanup is logged for retry.

## Capture admission

`clip-daemon-max-bytes` in the Ringboard data directory is an atomic decimal
configuration shared by server and watcher. Missing configuration defaults to
16 MiB; malformed values reject admission. The hard ceiling is 64 MiB.

The watcher uses bounded memory-backed staging for every MIME, drops oversized
transfers before persistence, and preserves upstream password-manager-hint
exclusion. The server snapshots each Add in bounded memory **before** eviction
or disk staging, protecting against direct clients as well. A rejected legacy
Add returns reserved ID `u64::MAX`; packaged CLI/watcher paths handle this rather
than claiming a retained entry exists. Memory-backed buffers may be swapped by
the OS; no no-swap guarantee is made.

Validation: `RINGBOARD_SERVER=/path/to/patched/ringboard-server python3
scripts/backend-regressions.py replacement wraparound`; the legacy rejection
case runs against an unpatched server. `concurrent-mutations` sends eight
simultaneous requests over independent sockets and verifies one winner and seven
stale failures, plus no partial deletion for an externally stale bulk target.
The replacement test covers the oldest
and newest entries in full main/favorite rings and preserves row order/count.
