# Phase 4 management and privacy coverage

## Enforced by clip-daemon

- Entry revision checks reject stale delete, favorite, restore, and paste actions.
- Paste sessions expire after 15 seconds, retain compositor targets only in daemon memory, and never expose raw addresses or titles.
- Delete and favorite changes use Ringboard's server protocol rather than writing database files.
- Wipe requires a one-use, 30-second challenge and clears regular history, favorites, thumbnails, temporary transfers, and pending annotation tasks.
- Pause and private mode stop `ringboard-wayland.service`; resuming starts it again. The visible API state is stored with user-only permissions.
- Retention changes write Ringboard's native versioned server configuration and restart the server/capture pair. Size limits are validated and persisted.
- Annotation output is accepted only when it is a decodable image no larger than 32 MiB. Temporary files and thumbnails are private and cancellable.
- Current entries can be pinned through the same favorite transaction.

## Ringboard and compositor boundaries

Ringboard 0.16.2 rejects offers carrying `x-kde-passwordManagerHint` and ignores Chromium internal MIME types before persistence. `clip-daemon` does not claim source-window or password-field detection because the Wayland data-control protocol does not reliably identify the offer owner.

Ringboard 0.16.2 has no configurable pre-write byte limit. `max_entry_bytes` is therefore a validated, persisted desired limit, not a claim that an arbitrary Wayland producer is stopped before Ringboard writes. Closing this gap requires the pre-write Ringboard patch identified in the architecture decision; the daemon still bounds reads, details, thumbnails, edited images, and materialized transfers.

Hyprland targets are revalidated by the compositor when the post-hide shortcut is sent. If the target disappeared, the item remains selected and a copy-only notification is shown. Unsupported compositors remain copy-only. Terminal classes are configurable only in code at this phase and use `Ctrl+Shift+V`; other targets use `Ctrl+V`.

Ringboard does not expose observable paste-completion acknowledgement. The API reports `paste-prepared` and the post-hide fallback dispatch, but does not claim application-level insertion completion.
