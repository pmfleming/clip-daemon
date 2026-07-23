# ADR 0001: Ringboard behind clip-daemon

- Status: provisional, qualification required
- Decision: use pinned Nixpkgs `ringboard-server`/`ringboard-wayland`; Shelllist talks only to `clip-api`.

The initial implementation reads Ringboard through `clipboard-history-client-sdk` 0.16.2 and exposes opaque IDs, bounded previews, semantic classification, a session D-Bus service, and a JSONL bridge. Mutations and paste remain disabled until the hardware qualification gate passes. This intentionally produces copy-only session metadata rather than unsafe input injection.

## Qualification record

Run `nix run .#qualify` from a Hyprland graphical session and record results in `docs/qualification-results.md`. This probe does not install services or alter `Super+V`.

Required manual cases: protocol availability; text/image/file MIME round trips; MIME priority; layer-shell focus restoration and auto-paste across Ghostty, browsers, VS Code, GTK and Qt; sensitive marker exclusion; pre-persistence size limit; clipboard ownership after source exit.

The Nix input is pinned by `flake.lock`. The reviewed engine package is `0.16.2-unstable-2026-05-10`; re-run the gate if that pin changes.
