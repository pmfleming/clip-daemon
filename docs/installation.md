# Installation (Wayland, systemd user session)

## Nix package

```sh
package=$(python3 ../daemon-framework/tools/local-build.py build . --no-link --print-out-paths)
ringboard=$(python3 ../daemon-framework/tools/local-build.py build --attr ringboard . --no-link --print-out-paths)
nix profile install "$package" "$ringboard"
```

Keep the current `daemon-framework` checkout alongside this repository. Cargo
and Nix builds use that same source, including tracked dirty files. The helper
snapshots sources per invocation without persisting local deployment pins.
Do not vendor or revision-pin the framework. The default package
contains all three service definitions and references the patched engine by
absolute store path. Satty, grim, hyprctl, service control, notifications and file
opening dependencies are wrapped into the daemon's runtime PATH. The separate
`ringboard` output adds the diagnostic CLI to your interactive PATH.

Before activation, back up existing Ringboard data and remove/disable conflicting
old `ringboard-server` / `ringboard-wayland` unit definitions in your NixOS or Home
Manager configuration. Do not run another clipboard capture watcher alongside
this one. The privacy API controls the named managed watcher, not arbitrary
processes started independently.

For a manually managed user installation, link the packaged units (review any
existing destination first; these commands deliberately do not overwrite it):

```sh
package=$(python3 ../daemon-framework/tools/local-build.py build . --no-link --print-out-paths)
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
for unit in clip-daemon ringboard-server ringboard-wayland; do
  ln -s "$package/share/systemd/user/$unit.service" \
    "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/$unit.service"
done
systemctl --user daemon-reload
# Run inside the intended compositor session, or arrange equivalent compositor
# startup environment import before graphical-session.target starts.
systemctl --user import-environment WAYLAND_DISPLAY HYPRLAND_INSTANCE_SIGNATURE
systemctl --user enable --now clip-daemon.service
```

Upgrades must update these explicit store-path links. Declarative configurations
should source these same packaged unit files rather than retaining old engine
commands. The profile exposes the installed D-Bus activation file through its
`share` directory; ensure the profile share directory is in `XDG_DATA_DIRS` at
session startup if your distribution does not arrange this automatically.

The server's pre-start command imports existing native retention (or fresh
750/100 defaults), validates preferences, and writes native/count/byte policy
before opening the rings. Its notify readiness orders the watcher and daemon.
The watcher's `ExecCondition` refuses startup when persisted pause/private intent
is set **or unreadable**. Thus a login or external service restart cannot briefly
capture while the daemon is still restoring privacy. Runtime requests still
verify service state and retry failed transitions.

No installation or test script automatically migrates, erases or restarts your
existing production history. Restarting the new engine with reduced retention
can discard old entries according to the requested limits; review settings first.
The engine extension is not a power-loss-ACID storage layer.

## Verify without touching production

```sh
package=$(python3 ../daemon-framework/tools/local-build.py build . --no-link --print-out-paths)
python3 ../daemon-framework/tools/local-build.py develop . --command python3 scripts/package-smoke.py "$package"
```

This checks installed unit syntax, private first-boot files, startup privacy
conditions, installed D-Bus activation, and live engine limits in a temporary HOME
and private bus. It does **not** start your user units or claim a full login/systemd
VM qualification. `just live-acceptance` additionally uses a nested compositor.

After deliberate production activation, inspect metadata-only health with
`clip-daemon probe-ringboard` and `clipboard.settings.get`; retention should be
synchronized and capture verified. Do not equate persisted preferences with
successful service control.

## Cargo development

For a fresh workspace, clone `https://github.com/pmfleming/daemon-framework`
alongside this repository. Use its current worktree; never check out an old
compatibility pin or reset an existing development checkout. Enter the shell via
`python3 ../daemon-framework/tools/local-build.py develop .`, then `just check`.

Stock Ringboard is read-compatible only: safe mutations and pre-write byte
admission require the supplied policy-enabled package. Server extension source
and AGPL license are shipped under `packaging/ringboard-policy/`; the daemon
itself remains MIT.
