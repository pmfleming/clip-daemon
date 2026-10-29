# Turso implementation stages

1. Engine contract — `3bbd47c`: newest stable release verified as 0.8.3;
   API/core/SDK pinned; isolated Rust SQL/FTS/large-BLOB/recovery probe passes.
2. Relational store — fresh schema, bounded owner worker, SQL pages/count/search,
   exact-byte dedup, revision-checked mutations, set-based retention and SQL
   artifact references. Twelve on-disk integration tests pass, including privacy
   admission, restart, owner/symlink rejection, corruption, atomic bulk deletion,
   races and full-body search. Native FTS trigram candidates plus SQL verification
   implement literal search. Wiring replaces the backend in stage 3; intermediate
   commits are intentionally not compatibility releases.

All evidence uses isolated temporary data. No production service was restarted
and no old desktop history was read, imported or deleted.
