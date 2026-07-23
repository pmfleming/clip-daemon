# Ringboard qualification results

Status: protocol probe run 2026-07-23; content/action hardware matrix pending

| Gate | Result | Notes |
|---|---|---|
| Required Wayland protocols | pass | Hyprland session exposes `ext_data_control_manager_v1`, `zwp_virtual_keyboard_manager_v1`, and foreign-toplevel; `ringboard-server` and `ringboard-wayland` 0.16.2 are present |
| Read-only SDK snapshot/query | pass | Database opened and a bounded query completed; database was empty during the probe |
| Text/image/file MIME round trip | pending | |
| File MIME priority | pending | |
| Layer-shell focus and auto-paste targets | pending | |
| Sensitive selections excluded | pending | |
| Pre-write maximum entry size | pending | Ringboard 0.16.2 config exposes entry counts; size cap still requires verification/patch |
| Clipboard survives source exit | pending | Do not remove `wl-clip-persist` yet |

No production watcher, paste owner, or `Super+V` binding has been changed.
