# Phase 4 management and privacy coverage

## Enforced by clip-daemon

- Entry IDs are bound to the observed Ringboard history generation and content fingerprint.
  Backend mutations revalidate the expected revision immediately before acting, including
  delayed annotation results.
- Paste sessions expire after five minutes, retain compositor targets only in daemon memory, and never expose raw addresses or titles. Automatic paste is armed only after the requested Wayland selection publication succeeds; publication failure leaves the session copy-only instead of pasting stale clipboard content.
- Wayland selection publication validates exact MIME values and applies the configured entry limit plus a 64 MiB hard ceiling before retaining bytes in the daemon-owned publisher.
- Delete and favorite changes use Ringboard's server protocol rather than writing database files.
- Wipe requires a one-use, 30-second challenge and clears regular history, favorites, thumbnails, temporary transfers, and pending annotation tasks.
- Pause and private mode stop `ringboard-wayland.service`; resuming starts it again. The visible API state is stored with user-only permissions.
- Retention changes stage and sync both daemon and Ringboard configuration files before
  atomic renames, roll back partial commits, skip no-op restarts, and report restart errors
  explicitly after a successful commit. Size limits are validated and persisted.
- Annotation output is accepted only when it is a decodable image no larger than 32 MiB.
  Screenshot requests additionally cap total area at 32 megapixels and terminate `grim` after 15 seconds.
  Image inspection and thumbnail decoding cap dimensions at 16,384 pixels per edge and decoded
  allocation at 128 MiB. A file-backed image is dereferenced only when its file-list or plain-text
  URI names exactly one local, non-symlink regular file that passes size, format, and dimension
  validation; each operation revalidates it. Temporary files and thumbnails are private and cancellable.
- Current entries can be pinned through the same favorite transaction.
- Image files materialized by `image-as-file` are recorded in a private persistent ownership
  registry. Reconciliation removes only registered, unreferenced files (after a short recapture
  grace period), preserves the active selection and Ringboard URI references, and never deletes
  unrelated files. Wipe removes all registered artifacts.
- Ringboard may recapture a daemon-published generated URI. The history projection verifies the
  generated file's image identity and, when `collapse_self_echoes` is enabled, maps an equivalent
  echo back to its still-present source entry. Ringboard capture and storage remain unchanged.

## Ringboard and compositor boundaries

The exported session D-Bus interface does not apply per-caller authorization. Every process in
the user's session-bus trust domain can read clipboard content and invoke destructive or launch
actions, matching the trust level of processes that can already read the user's Ringboard data.

Ringboard 0.16.2 rejects offers carrying `x-kde-passwordManagerHint` and ignores Chromium internal MIME types before persistence. `clip-daemon` does not claim source-window or password-field detection because the Wayland data-control protocol does not reliably identify the offer owner.

Ringboard 0.16.2 has no configurable pre-write byte limit. `max_entry_bytes` is therefore a validated, persisted desired limit, not a claim that an arbitrary Wayland producer is stopped before Ringboard writes. Closing this gap requires the pre-write Ringboard patch identified in the architecture decision; the daemon still bounds reads, details, thumbnails, edited images, and materialized transfers.

Hyprland targets are revalidated by the compositor when the post-hide shortcut is sent. If the target disappeared, the item remains selected and a copy-only notification is shown. Unsupported compositors remain copy-only. Terminal classes are configurable only in code at this phase and use `Ctrl+Shift+V`; other targets use `Ctrl+V`.

Wayland selection ownership does not expose target-application paste acknowledgement. The API reports `paste-prepared` after the compositor accepts the selection and requests picker hiding, but it does not claim application-level insertion completion.
