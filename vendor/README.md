# Shared matcher

`shelllist-search/` is an unmodified source snapshot of
`../../shelllist/rust/shelllist-search/{Cargo.toml,src}` in sibling checkouts.
The canonical matcher stays with Shelllist; clip-daemon uses its library API so
clipboard search retains identical normalization, typo tolerance and ranking.
No QML/runtime dependency is introduced. Refresh this snapshot whenever the
canonical matcher changes; the sibling boundary gate checks source equality.
