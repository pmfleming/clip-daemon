# Clipboard transport ownership prerequisite

The Unix JSONL migration remains incomplete. This stage fixes existing producer
ownership before exposing another ingress. There is no clipboard socket listener
or frontend socket opt-in in this stage; the retained D-Bus/client path remains.

## Implemented boundary

- Paste sessions, text edit leases and wipe challenges belong to the caller's
  D-Bus unique name. Trusted in-process calls with no owner form a separate scope;
  `None` is not a wildcard for a named caller's resources.
- Foreign session validation/arming/hidden acknowledgement and edit commit fail
  before publication. Foreign end/edit-cancel are idempotent no-ops and cannot
  consume the rightful owner's resource. A foreign wipe attempt cannot consume
  another owner's single-use challenge.
- Edit-and-paste requires both the lease and the paste session to belong to the
  caller. Existing revision/size checks and publication-before-arming remain.
- Hidden acknowledgement is single-use. Its delayed paste task stays attached to
  the session and is aborted on end, owner loss, expiry cleanup or manager drop.
  No recovery path invokes hidden, publishes content or starts another paste.
  Aborting a helper does **not** undo input already injected or clipboard content
  already published; an interrupted effect remains uncertain.
- The service observes owner loss before exporting its interface. Loss releases
  only that owner's sessions, edits and challenges. Lag reconciles owners against
  the private bus; monitor failure or shutdown closes admission and releases all
  named resource owners. Accepted calls retain their admission while finishing;
  a post-dispatch liveness check releases resources registered after an earlier
  disconnect/cleanup. Unrelated owners remain usable.
- API-generated session, screenshot and synchronous operation receipts are
  filtered by owner before signal delivery. Capture pause/private-mode
  configuration remains shared state. Raw D-Bus Publish receipts retain the
  caller's identity too. History invalidations remain shared.

## Remaining migration blockers

This is **not** complete operation/capture ownership or recovery:

- Backend asynchronous operation broadcasts still need owner-scoped delivery,
  registration/terminal ordering and disconnect cleanup. Existing CLI screenshot
  behavior has not been changed to cancel accepted background work on CLI exit.
- No authoritative operation/capture/session status or retained terminal API has
  been added. Lost outcomes remain unknown; settings are not completion evidence.
- History content tokens are equality identities, not delivery counters. Initial,
  lag/reset and query races still need the planned frontend/producer recovery work.
- Per-owner filtering does not introduce wire ordering metadata. Broadcast lag may
  conservatively notify subscribers whose own lifecycle records were unaffected.
- Listener adoption, frontend opt-in, native Quickshell acceptance, clipboard
  maximum payload/slow-reader tests, process-crash/load/performance and startup
  acceptance remain outstanding. These tests do not authorize rollout.

## Evidence and reproduction

New producer coverage exercises foreign/unowned callers, edit-and-paste ordering,
peer-preserving release, single-use hidden acknowledgement and cancellation of a
fake delayed paste task. Private real-D-Bus tests exercise the exported service,
receipt isolation, reconnect rejection, peer survival and a deliberately blocked
edit read whose lease is registered after owner loss. The late-registration case
also covers owner-monitor failure and explicit shutdown. Fixtures never initialize
capture or run clipboard/hardware helpers. They are not native socket evidence.

Run from the repository in its Nix development shell, with private paths and no
host bus access (the D-Bus fixtures create their own temporary bus):

```sh
nix develop . --no-write-lock-file
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
private=$(mktemp -d /tmp/clip-owner-test.XXXXXX)
export HOME="$private/home" XDG_CONFIG_HOME="$private/config"
export XDG_CACHE_HOME="$private/cache" XDG_RUNTIME_DIR="$private/runtime"
mkdir -p "$HOME" "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export DBUS_SESSION_BUS_ADDRESS=unix:path=/nonexistent-clip-test
export DBUS_SYSTEM_BUS_ADDRESS=unix:path=/nonexistent-clip-system
export WAYLAND_DISPLAY=nonexistent-clip-wayland
export HYPRLAND_INSTANCE_SIGNATURE=nonexistent-clip-hyprland
cargo test --locked --all-targets --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
# Remove only this generated fixture directory after leaving the test shell.
```

Recorded debug validation: 48 library passes / one existing benchmark ignored,
five API integration passes and one retained-client integration pass. Strict
all-target/all-feature Clippy and formatting pass. Shelllist's unchanged frontend
suite passes 419 cases / one existing renderer skip; strict QML lint, generated
contracts, surface catalog and build-input checks pass. Sibling-source Nix
`clipDaemonContract`, `qmlTests` and `buildInputs` pass; the release package runs
48 library, five API and one retained-client integration test. Its private bus
has a checked-in no-activation configuration and an explicit `nativeCheckInputs`
D-Bus executable dependency (both required by the isolated build).

No live service restart, real clipboard publication, real cache deletion or
default transport change.
