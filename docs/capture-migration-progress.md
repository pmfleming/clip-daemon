# Capture migration progress

Implementation follows [the migration plan](wayland-capture-migration.md).
Production services are not changed by these development commits.

## Stage 1 — contract and design

- Accepted ADR 0003 with explicit regular-only capture, startup/resume semantics,
  ext/wlr protocol selection, privacy barriers, budgets, and licensing boundaries.
- Added 14 executable SDK MIME fixtures, including sensitive-marker ordering,
  GNOME cut payloads, text aliases, image priority, and Chromium exclusions.
- Enabled the SDK's Apache-2.0 watcher-utils API; no copied upstream source.
- Validation: `cargo test --offline --test capture_contract` passed. Existing
  backend/desktop suites remain the integration baseline for later stages.
- Real compositor fallback, multi-seat, and deployment qualification are future
  gates, not claimed by this stage.
