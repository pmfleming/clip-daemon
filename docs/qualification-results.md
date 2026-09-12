# Ringboard qualification results

Historical production probe: 2026-07-25. Physical hardware records remain tracked
by `just hardware-acceptance`; see the remediation section below for current
nested-desktop qualification.

| Gate | Result | Notes |
|---|---|---|
| Required Wayland protocols | pass | Rerun on 2026-07-25: Hyprland `wayland-1` exposes `ext_data_control_manager_v1`, `zwp_virtual_keyboard_manager_v1`, and foreign-toplevel; `ringboard-server` and `ringboard-wayland` 0.16.2 are present |
| Read-only SDK snapshot/query | pass | Rerun on 2026-07-25: database opened and a bounded 10-entry query completed with additional history reported while Ringboard services were active |
| Text MIME capture/read | partial pass | A `text/plain` `wl-copy` selection was captured and visible to the SDK query; restoration/paste is implemented but still awaits a recorded hardware round trip |
| Image/file MIME round trip | pending | |
| File MIME priority | pending | |
| Layer-shell focus and auto-paste targets | pending | |
| Sensitive selections excluded | pending | |
| Pre-write maximum entry size | pending | Patched capture admission now passes isolated tests; production deployment/hardware record still pending |
| Clipboard survives source exit | pending | Do not remove `wl-clip-persist` yet |

The 2026-07-25 probe also confirmed `/run/user/1000` and a readable clipboard-history database. It was intentionally read-only, so MIME/action, sensitive-data, focus, size-limit, and source-exit gates remain pending a hardware run.

Record each hardware result with `scripts/hardware-acceptance.sh record CHECK pass|fail|blocked "notes"`. Results are kept privately under `$XDG_STATE_HOME/clip-daemon/hardware-acceptance.tsv`; no clipboard content is recorded. The acceptance command combines these records with the protocol probe so pending gates remain explicit.

No production watcher, paste owner, or `Super+V` binding has been changed.

## Automated isolated live acceptance (2026-09-08)

`cargo build --locked` followed by `python3 scripts/isolated-acceptance.py` runs a real nested Hyprland 0.55.4, Ringboard 0.16.2 server/capture pair, the locally built daemon, a GTK3 paste target, and the real Satty editor. Requirements are listed in the script; Python needs GTK3 introspection typelibs on `GI_TYPELIB_PATH`. All HOME/XDG paths, Ringboard sockets, and D-Bus are disposable. The harness checks the daemon PID and initially empty history before mutations, and kills its children afterward. Private diagnostic logs and JSON results are under `target/live-acceptance/`.

All 11 checks pass: isolated protocols and empty database, text capture/copy after producer exit, favorite/unfavorite, compositor-targeted paste after hiding, exact image round trip, Satty save/completion, Satty Escape cancellation without a revision change, dual-MIME cut-file publication, bulk delete, and two-phase wipe.

The first run exposed a real GTK paste failure: Ringboard normalizes captured text to `text/plain`, but GTK requests UTF-8 text aliases. Publication now retains the original offer and adds standard aliases only for valid UTF-8 `text/plain` / `text/plain;charset=utf-8`. Images, file lists, non-UTF-8 text, and other charsets remain exact-MIME-only. A unit regression test and the live GTK paste test cover this fix. The full Rust suite has 33 passing tests; strict Clippy passes.

That historical run did not cover Shelllist, terminals, or capture admission.

## Remediation qualification

**All 17 checks pass** with the packaged policy-enabled Ringboard, real Ghostty,
and actual Shelllist 0.2.0 / Quickshell 0.3.0 in the disposable nested Hyprland.
Added checks cover terminal Ctrl+Shift+V, actual Shelllist layer-shell selection
and hiding followed by paste into both GTK and Ghostty, safe copy-only behavior
with no target, sensitive-marker rejection, and pre-persistence oversized-offer
rejection. No synthetic replacement of the Shelllist picker/controller is used;
non-clipboard daemons are stubbed so their host services cannot be touched.

To repeat the full matrix, set the paths for the Shelllist version to qualify:

```sh
export SHELLLIST_QML_ROOT=/path/to/shelllist-config/share/shelllist
export SHELLLIST_SEARCH=/path/to/shelllist-search/bin/shelllist-search
# QUICKSHELL and GHOSTTY may override the development-shell executables.
just live-acceptance
```

Without Shelllist inputs the runner explicitly prints NOT RUN for those two
checks; the other **15** checks still execute. Client/package paths are recorded
in `target/live-acceptance/clients.json` alongside results and synthetic logs.

The patched engine also passes 11 isolated storage/policy regression cases. Both
Nix packages build; clean-HOME installed-unit/D-Bus smoke tests pass. See
`docs/robustness-remediation.md` and `docs/installation.md` for scope.

These are real clients on a nested desktop, **not physical keyboard/login or
production-service acceptance**. Original manual hardware records are not
rewritten, and no production watcher, history, binding or clipboard owner was
changed. Unmarked sensitive sources, OS swapping and power-loss durability remain
explicit limitations rather than certified guarantees.
