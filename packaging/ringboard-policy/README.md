# Ringboard policy extension v1

The Nix `ringboard` output applies this narrowly scoped patch to the pinned
Ringboard 0.16.2 source. The added server code is **AGPL-3.0-only**, matching
Ringboard's server license. The daemon remains MIT licensed and communicates
over a socket. No database files are written by the daemon.

Legacy Ringboard clients retain protocol 0. Policy clients negotiate byte `0xc1`
instead. An unpatched server returns version 0 and closes the connection; the
daemon refuses to send any mutating request in that case. Never send extension
packets to a server without a successful policy handshake.

Packets: `CDP1`, operation byte (1 = compare-and-replace), raw entry ID (u64 LE),
32-byte expected content proof, MIME length (u8, maximum 96), MIME bytes. Replace
also passes one regular file descriptor using SCM_RIGHTS. Replies are `CDR1`
plus status: 0 committed, 1 stale/missing, 2 rejected/error. IPC waits are bounded.
A lost reply is an uncertain outcome: refresh history before retrying.

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

Validation: `RINGBOARD_SERVER=/path/to/patched/ringboard-server python3
scripts/backend-regressions.py replacement wraparound`; the legacy rejection
case runs against an unpatched server. The replacement test covers the oldest
and newest entries in full main/favorite rings and preserves row order/count.
