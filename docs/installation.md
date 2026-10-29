# Single-service Turso installation and fresh-start cutover

This is a deliberate breaking storage change. **Old clipboard history and
favorites are discarded from the application view.** The new daemon never reads
Ringboard files, imports old identities or preserves a legacy backend. There is
no data rollback promise. No implementation or acceptance command here activates
production services automatically.

## Build first

Keep `daemon-framework` alongside this repository:

```
../daemon-framework/tools/local-build check .
../daemon-framework/tools/local-build build . --no-link --print-out-paths
```

The package installs only `clip-daemon.service` and its D-Bus activation file.
It embeds Turso; there is no `ringboard` output, policy-server package, bootstrap
flag or second collector. Satty/grim/slurp/hyprctl and file-opening tools remain
wrapped runtime helpers, not storage services. The retired Yazi Lua plugin is no
longer shipped; file-manager callers can use the Rust `selection.publishFiles`
API instead.

Rebuild Shelllist too: `ClipboardBackend.qml` now sends `cursor_mode: true`, not
`fuzzy: true`. The cursor API delegates directly to SQL search and pagination.
The companion Home Manager change in `nixos-capture/home.nix` sources only the
clipboard daemon unit and masks both retired Ringboard units.

## Disposable rehearsal

```
just storage-contract
just backend-regressions
just live-acceptance
just package-acceptance /nix/store/REVIEWED-clip-daemon
```

All new harnesses are Rust. They use private HOME/XDG paths, a private bus and an
isolated compositor. They do not call the production systemd manager, import
normal session state, delete old clipboard history or run an old engine.
Keep diagnostics private: failures retain isolated synthetic-data logs only.

## Production cutover (explicit operator action)

1. Finish tests and review the new daemon/Shelllist/Home Manager builds before
   stopping anything. Expect an intentional clipboard capture gap.
2. Stop `clip-daemon.service`, `ringboard-wayland.service` and
   `ringboard-server.service`. Remove other compositor autostarts or standalone
   clipboard collectors; prevent D-Bus activation from restarting the old facade
   during the package switch.
3. Deploy the reviewed package and QML caller together. Remove the old server and
   watcher enablement links; mask both retired units. Apply the companion Home
   Manager configuration rather than retaining copied old unit commands.
4. Old `${XDG_DATA_HOME:-$HOME/.local/share}/clipboard-history` files can remain
   unused. If deliberately deleting them, confirm the exact directory and that
   all old writers are stopped. The new program never performs this deletion.
   Do not delete settings, arbitrary copied URI files or unproven old artifacts.
5. Reload the user manager and import the intended `WAYLAND_DISPLAY` and
   `HYPRLAND_INSTANCE_SIGNATURE`. Start **only** `clip-daemon.service`.
   Its unit conflicts with both retired services; no server is wanted/required.
6. Confirm the deployed executable is the reviewed Turso build and
   `clipboard.settings.get` reports synchronized effective retention. The new
   database is initially empty. Old IDs cannot address new UUID records.
7. Missing preferences start paused; explicitly resume through the clipboard UI.
   Invalid preferences keep capture unavailable until the file is corrected.
   Valid saved private/paused preferences remain in effect. Confirm verified
   capture state rather than relying on a desired preference alone.
8. Copy synthetic text, search, edit, favorite, delete and restart normally.
   Verify new history persists, then test pause/private/resume. Content copied
   while paused must not appear merely because capture resumes.

New store:
`${XDG_DATA_HOME:-$HOME/.local/share}/clip-daemon/history/history.db`.
Directory mode is 0700; database/lock/WAL are 0600. One worker owns an exclusive
file lock and verifies application/schema identity. Wrong schema, corruption,
unsafe paths and ordinary I/O failures **do not trigger deletion or fallback**.
An incompatible development store requires an explicit scoped offline reset;
never remove a main file alone while WAL/sidecars or an owner remain active.

## Policy

Defaults are 1,000 regular entries, 100 favorites, 16 MiB per entry and 256 MiB
retained payload. Settings support at most 100,000 regular entries, 10,000
favorites and 64 MiB per entry. Actual operational performance at extreme counts
is distinct from validation ceilings; see qualification measurements.

Privacy transitions stop capture, fence submissions and apply policy directly
on the database owner. No service restart or native Ringboard configuration is
involved. Shutdown closes capture, drains editors/accepted storage work and
checks the WAL checkpoint. Failed or uncertain persistence never permits blind
replay. Publication and editing can remain usable while capture is paused.

The default systemd stop timeout is 30 seconds. A forced kill can recover
acknowledged WAL commits, but an unacknowledged operation is not permission to
assume either failure or success. Diagnose the store before retrying.
