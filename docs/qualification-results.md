# Ringboard qualification results

Status: protocol probe rerun 2026-07-25; content/action hardware matrix pending

| Gate | Result | Notes |
|---|---|---|
| Required Wayland protocols | pass | Rerun on 2026-07-25: Hyprland `wayland-1` exposes `ext_data_control_manager_v1`, `zwp_virtual_keyboard_manager_v1`, and foreign-toplevel; `ringboard-server` and `ringboard-wayland` 0.16.2 are present |
| Read-only SDK snapshot/query | pass | Rerun on 2026-07-25: database opened and a bounded 10-entry query completed with additional history reported while Ringboard services were active |
| Text MIME capture/read | partial pass | A `text/plain` `wl-copy` selection was captured and visible to the SDK query; restoration/paste is not implemented yet |
| Image/file MIME round trip | pending | |
| File MIME priority | pending | |
| Layer-shell focus and auto-paste targets | pending | |
| Sensitive selections excluded | pending | |
| Pre-write maximum entry size | pending | Ringboard 0.16.2 config exposes entry counts; size cap still requires verification/patch |
| Clipboard survives source exit | pending | Do not remove `wl-clip-persist` yet |

The 2026-07-25 probe also confirmed `/run/user/1000` and a readable clipboard-history database. It was intentionally read-only, so MIME/action, sensitive-data, focus, size-limit, and source-exit gates remain pending a hardware run.

No production watcher, paste owner, or `Super+V` binding has been changed.
