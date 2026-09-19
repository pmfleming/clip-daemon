# Installation (Wayland, systemd user session)

## Two-service architecture

`clip-daemon.service` owns regular Wayland capture, publication and the Shelllist
API. `ringboard-server.service` owns storage and atomic mutations. The package
ships **two** units and references its policy-v2 engine by absolute store path.
`ringboard-wayland.service` is retired; do not run a second clipboard watcher.
Primary (mouse-selected) text is not captured.

The daemon unit conflicts with the legacy watcher and orders its stop before
startup. Do not add `Requires=`/`PartOf=ringboard-server.service` to the daemon:
retention changes restart the engine while the facade remains alive to report the
outcome. Missing engine/Wayland support leaves capture unavailable, not falsely
running; the API stays available for diagnostics.

## Build

```sh
package=$(python3 ../daemon-framework/tools/local-build.py build . --no-link --print-out-paths)
ringboard=$(python3 ../daemon-framework/tools/local-build.py build --attr ringboard . --no-link --print-out-paths)
nix profile install "$package" "$ringboard"
```

Keep the current `daemon-framework` checkout alongside this repository. Cargo and
Nix use the same current source, including tracked dirty files; the helper creates
per-invocation snapshots without persistent local revision pins. The separate
Ringboard output provides the diagnostic CLI. Its legacy watcher binary is retained
for rollback qualification, **not** enabled or invoked by the new deployment.

Satty, grim, hyprctl, notifications, engine service control and file-opening tools
remain in the daemon's wrapped runtime PATH. Data-control requires a supported
Wayland compositor: ext-data-control is preferred, wlr-data-control v2 is the
fallback. The current recorded desktop qualification uses Hyprland/ext.

## Deliberate migration from three services

Do not activate blindly over a running watcher. No migration/test script controls
production services or erases/migrates production history automatically.

1. Retain the previous package closure and declarative generation for rollback.
   Inspect saved retention and pause/private intent before changing anything.
   Lowering retention on engine startup can discard entries.
2. Stop the old managed watcher, then the facade and engine. Check compositor
   autostarts and standalone processes for other collectors as well. This creates
   a deliberate capture gap rather than overlapping collectors.
3. With all history writers stopped, make a private backup of
   `${XDG_DATA_HOME:-$HOME/.local/share}/clipboard-history` and
   `${XDG_STATE_HOME:-$HOME/.local/state}/clip-daemon`. Preserve ownership/modes and
   protect the backup as sensitive clipboard data. Do not print its contents.
4. Update the declarative configuration to source only the two packaged units;
   remove the old watcher enablement and mask its unit. The supplied NixOS
   integration does this through Home Manager. For a manually managed installation,
   review/remove old unit and enablement links before linking the new files; mask
   `ringboard-wayland.service` so an old profile cannot reactivate it.
5. Reload the user manager, import the intended compositor environment, then enable
   and start the facade/engine. Verify the effective executable paths and that only
   one managed collector exists.
6. Check `clipboard.settings.get`: retention must be synchronized; capture must be
   verified and match saved intent. Paused/private startup must remain paused.
   Make one synthetic copy/edit and confirm it is captured only once.

Fresh/manual unit linkage (deliberately does not overwrite existing destinations):

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
for unit in clip-daemon ringboard-server; do
  ln -s "$package/share/systemd/user/$unit.service" \
    "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit.service"
done
systemctl --user daemon-reload
systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE
systemctl --user enable --now clip-daemon.service ringboard-server.service
```

Upgrades must replace explicit store-path links with the new matching package;
declarative configurations should source those same packaged units rather than
copy their commands. The profile exposes the D-Bus activation file through `share`;
ensure it is in `XDG_DATA_DIRS` at session startup.

The engine's `Type=notify`/`configure-engine` startup imports existing native
retention (fresh defaults: 750/100), validates preferences and writes count/byte
policy before opening rings. The facade starts with admission closed, validates
settings and engine capabilities, and acknowledges capture only after Wayland is
ready. Pause/private mode are in-process cancellation/submission barriers, not
watcher service-state guesses. The old `capture-allowed` command and opt-in
migration flag have been removed.

Resuming after pause skips bootstrap/current-selection snapshots rather than
importing content copied during the private interval. Copy again to capture it.
Publication and explicit edits remain usable while automatic capture is paused.
An uncertain storage reply stops ingestion and is not blindly retried. Investigate
engine health/history before deliberately restarting the facade to recover.

## Verify without touching production

```sh
package=$(python3 ../daemon-framework/tools/local-build.py build . --no-link --print-out-paths)
python3 ../daemon-framework/tools/local-build.py develop . --command python3 scripts/package-smoke.py "$package"
just live-acceptance
```

`package-smoke.py` checks the two installed units, legacy conflict, private files,
malformed-policy rejection, installed D-Bus activation, private startup and live
v2 negotiation using a disposable HOME/bus/history. An optional second argument is
a previous package closure; it checks engine rollback/upgrade against synthetic
history and retained private intent. It is **not** a real login/systemd VM test.

`python3 scripts/capture-acceptance.py` inside the development shell additionally
checks pause/resume, corrupt settings, failed retention recovery, wipe fencing and
compositor reconnect in a disposable nested desktop. `just live-acceptance` checks
the existing image/file/paste/editor behavior without an external watcher.

## Rollback

Stop the new facade/collector and engine before restoring the previous matching
three-service closure. Restore old declarative units (including removal of the
watcher mask), preserve saved pause/private intent, and verify the old watcher's
privacy startup condition before starting it. Never run both collectors together.
No database schema migration is involved; rollback does not reverse retention
eviction. Restore backups only deliberately with all writers stopped.

## Cargo development and licensing

Clone `https://github.com/pmfleming/daemon-framework` alongside this repository and
use its current worktree; do not reset it to a compatibility pin. Enter with
`python3 ../daemon-framework/tools/local-build.py develop .`, then `just check`.

Stock Ringboard is read-compatible only. Capture ingestion requires the packaged
v2 protocol; safe mutations and server pre-write admission remain mandatory.
Server extension source and AGPL license are in `packaging/ringboard-policy/`;
the facade remains MIT and uses the Apache-2.0 SDK MIME selector as a dependency.
