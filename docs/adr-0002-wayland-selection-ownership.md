# ADR 0002: clip-daemon owns Wayland clipboard restoration

- Status: accepted
- Decision: keep Ringboard as the capture/history engine and publish restored selections directly from `clip-daemon` through Wayland data control.
- Supersedes: the selection-ownership portion of ADR 0001.

## Context

Ringboard supports both X11 and Wayland and is maintained outside this project. Its 0.16.2 client SDK reads direct-entry MIME xattrs through an invalid `BorrowedBuf` transition on current Rust, and its paste helper requires a private paste-server wire protocol. Depending on that path made image restoration panic and coupled `clip-daemon` to behavior it cannot control.

Shelllist targets Hyprland/Wayland. It already delegates target capture, picker-hide acknowledgement, paste shortcut dispatch, validation, operation reporting, and cancellation policy to `clip-daemon`.

## Decision

`clip-daemon` now owns the regular Wayland clipboard selection with `wl-clipboard-rs` and the compositor's ext/wlr data-control protocol. It reads bounded content from a revision-validated Ringboard entry, validates its MIME, and publishes exactly that MIME. The publisher thread remains in the daemon process until another selection replaces it.

Image-as-file materialization writes a collision-safe private file, records its ownership and source identity in daemon state, and offers both `text/uri-list` and a plain-text URI fallback so file-aware and text-only paste targets can consume it. If history already contains one safe local image file, `clip-daemon` reuses that file instead of making another copy. The daemon reconciles its registry against history, safely prunes only unreferenced generated files, and can collapse verified recaptured URI and annotation echoes into their sources in the API projection. Raw images and these file-backed images share one semantic paste policy: default paste publishes image bytes, while the alternate action publishes a file URI. Screenshots and completed image annotations are also published directly. Ringboard continues to observe these selections through its normal Wayland capture path; `clip-daemon` does not add a second history watcher.

Ringboard remains responsible for capture, persistent history, favorites, deduplication, retention, and history mutation. Its server protocol remains in use for add/swap/remove operations needed to preserve history position, but its paste server and SDK paste helper are no longer used.

## Safety and lifecycle

- Published content is bounded by the configured `max_entry_bytes` and a 64 MiB hard Wayland-publishing ceiling.
- MIME values must be bounded, control-free ASCII media types.
- Text compatibility aliases are intentionally omitted so the selection exposes the stored MIME exactly.
- Paste sessions expire after five minutes. Hyprland revalidates the target when the post-hide shortcut is dispatched.
- Selection publication reports completion only after Wayland accepts ownership. The daemon does not claim that the target application consumed the data.
- Replacing or clearing the selection cancels the previous publisher through the Wayland data-source cancellation event. Daemon shutdown ends all publisher threads.
