# Development storage

The host's Rust `storage-daemon` owns development cache cleanup. Open
`shelllist storage` (or Storage from Updates) to scan usage, inspect eligible
artifacts, adjust aggregate budgets and retention, pin projects, or clean now.
Automatic deletion defaults to shutdown, after user sessions stop.

The default budgets are 64 GiB across project caches and 8 GiB across shared
caches, with 24-hour warm protection and 14-day inactive retention. Generated
Cargo profiles in custom build directories are included. Source files, tracked
files, worktrees and active builds are preserved. Reports and unmarked logs are
measured only. Nix GC and system temporary files keep their existing policies.

`storage-daemon scan` prints a read-only inventory using the host configuration.
The former per-checkout Python cleaner and legacy trim aliases are removed.
No cleanup runs on development-shell entry. See the sibling storage-daemon
`contracts/storage.md` for safety, completed-log ownership and API details.
