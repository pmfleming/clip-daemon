# Scoped FTS compression dependency patch

`tantivy-sstable/` is the published MIT-licensed `tantivy-sstable` **0.7.0**
source used by Turso 0.8.3's Tantivy 0.26 dependency. Its Rust sources and tests
are unchanged. The manifest changes are `zstd = "0.13"` → `"0.14.3"`
and removal of its unused `names` development dependency.
The upstream MIT license is included from the Tantivy 0.26.2 release.

Why: RustSec RUSTSEC-2026-0391 affects zstd 0.13.3; 2026-0403 and 2026-0398
affect its zstd-safe 7.3.0 dependency. The refreshed lockfiles use released
zstd 0.14.3 / zstd-safe 8.2.0. Simply updating the lock cannot cross upstream's
0.13 requirement. No advisory is suppressed. Both the application and isolated
engine probe use the same patch, which is included in the Nix source.

Remove this patch when a stable Turso release permits a Tantivy version with the
fixed compression range. This is not a Ringboard/data compatibility adapter.

`cargo audit` still reports upstream **warnings**, without suppressions:
Tantivy's lru 0.16 panic-safety advisory and the zbus_names/zvariant/
zvariant_derive maintenance advisories. The shared desktop D-Bus dependency
migration is separate from replacing clipboard storage. Audit does not currently
report a vulnerability error for the shipped graph.
