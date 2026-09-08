# Ringboard qualification results

Status: protocol probe rerun 2026-07-25; content/action hardware matrix tracked by `just hardware-acceptance`

| Gate | Result | Notes |
|---|---|---|
| Required Wayland protocols | pass | Rerun on 2026-07-25: Hyprland `wayland-1` exposes `ext_data_control_manager_v1`, `zwp_virtual_keyboard_manager_v1`, and foreign-toplevel; `ringboard-server` and `ringboard-wayland` 0.16.2 are present |
| Read-only SDK snapshot/query | pass | Rerun on 2026-07-25: database opened and a bounded 10-entry query completed with additional history reported while Ringboard services were active |
| Text MIME capture/read | partial pass | A `text/plain` `wl-copy` selection was captured and visible to the SDK query; restoration/paste is implemented but still awaits a recorded hardware round trip |
| Image/file MIME round trip | pending | |
| File MIME priority | pending | |
| Layer-shell focus and auto-paste targets | pending | |
| Sensitive selections excluded | pending | |
| Pre-write maximum entry size | pending | Ringboard 0.16.2 config exposes entry counts; size cap still requires verification/patch |
| Clipboard survives source exit | pending | Do not remove `wl-clip-persist` yet |

The 2026-07-25 probe also confirmed `/run/user/1000` and a readable clipboard-history database. It was intentionally read-only, so MIME/action, sensitive-data, focus, size-limit, and source-exit gates remain pending a hardware run.

Record each hardware result with `scripts/hardware-acceptance.sh record CHECK pass|fail|blocked "notes"`. Results are kept privately under `$XDG_STATE_HOME/clip-daemon/hardware-acceptance.tsv`; no clipboard content is recorded. The acceptance command combines these records with the protocol probe so pending gates remain explicit.

No production watcher, paste owner, or `Super+V` binding has been changed.

## Automated isolated live acceptance (2026-09-08)

`cargo build --locked` followed by `python3 scripts/isolated-acceptance.py` runs a real nested Hyprland 0.55.4, Ringboard 0.16.2 server/capture pair, the locally built daemon, a GTK3 paste target, and the real Satty editor. Requirements are listed in the script; Python needs GTK3 introspection typelibs on `GI_TYPELIB_PATH`. All HOME/XDG paths, Ringboard sockets, and D-Bus are disposable. The harness checks the daemon PID and initially empty history before mutations, and kills its children afterward. Private diagnostic logs and JSON results are under `target/live-acceptance/`.

All 11 checks pass: isolated protocols and empty database, text capture/copy after producer exit, favorite/unfavorite, compositor-targeted paste after hiding, exact image round trip, Satty save/completion, Satty Escape cancellation without a revision change, dual-MIME cut-file publication, bulk delete, and two-phase wipe.

The first run exposed a real GTK paste failure: Ringboard normalizes captured text to `text/plain`, but GTK requests UTF-8 text aliases. Publication now retains the original offer and adds standard aliases only for valid UTF-8 `text/plain` / `text/plain;charset=utf-8`. Images, file lists, non-UTF-8 text, and other charsets remain exact-MIME-only. A unit regression test and the live GTK paste test cover this fix. The full Rust suite has 33 passing tests; strict Clippy passes.

This does not certify the actual Shelllist layer-shell picker, terminal-specific Ctrl+Shift+V, sensitive-source exclusion, or Ringboard's pre-capture size enforcement. Those original hardware-matrix rows remain pending; no unrelated gate is marked passed.
