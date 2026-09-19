# Phase 4 management and privacy coverage

## Enforced by clip-daemon

- Entry IDs are bound to the observed Ringboard history generation and content fingerprint.
  Backend mutations revalidate the expected revision immediately before acting, including
  delayed annotation results.
- Paste sessions expire after five minutes, retain compositor targets only in daemon memory, and never expose raw addresses or titles. Automatic paste is armed only after the requested Wayland selection publication succeeds; publication failure leaves the session copy-only instead of pasting stale clipboard content.
- Wayland selection publication validates exact MIME values and applies the configured entry limit plus a 64 MiB hard ceiling before retaining bytes in the daemon-owned publisher.
- Delete and favorite changes use Ringboard's server protocol rather than writing database files.
- Wipe requires a one-use, 30-second challenge and a verified capture fence before clearing regular history, favorites, thumbnails, temporary transfers, and pending annotation tasks. Pre-wipe offers cannot refill history after recovery. A failed fence prevents deletion; failed capture recovery after a committed wipe is reported as a warning.
- Multi-delete accepts at most 5,000 unique entry ID/revision pairs and validates the complete selection before removing any entry. This keeps stale UI selections from causing an avoidable partial delete.
- Pause and private mode close in-process admission, invalidate generations, cancel transfers, fence submissions and join capture workers before acknowledgement. A timeout, lost storage reply or persistence failure never counts as verified privacy. Intent is stored with user-only permissions; publication remains independent. Resume/reconnect discards bootstrap selections rather than replaying a private backlog.
- Retention changes stage and sync both daemon and Ringboard configuration files before
  atomic renames, roll back partial commits, skip no-op restarts, and report restart errors
  explicitly after a successful commit. Capture is quiesced before the transition and
  resumes only after live engine readiness and negotiated limits match saved policy.
  Size-only changes also update the collector without restarting the engine.
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
- The collector may capture a daemon-published generated URI. The history projection verifies the
  generated file's image identity and, when `collapse_self_echoes` is enabled, maps an equivalent
  echo back to its still-present source entry. This is not a blanket exclusion of
  daemon publications. Text edits use Ringboard's plain-text MIME normalization so
  restoring an edit deduplicates without creating a MIME-distinct echo entry.

## Ringboard and compositor boundaries

The exported session D-Bus interface does not apply per-caller authorization. Every process in
the user's session-bus trust domain can read clipboard content and invoke destructive or launch
actions, matching the trust level of processes that can already read the user's Ringboard data.

The daemon collector rejects whole offers carrying `x-kde-passwordManagerHint` and ignores Chromium internal MIME types before requesting payloads. `clip-daemon` does not claim source-window or password-field detection because the Wayland data-control protocol does not reliably identify the offer owner.

Capture uses bounded memory-backed files, at most four transfers, a queue of one,
128 MiB aggregate payload reservations, and 5/15-second idle/total deadlines. Both
collector and server enforce `min(max_entry_bytes, 64 MiB)`. The server independently
admits every Add/capture before disk staging or retention eviction; capture v2
atomically validates content/MIME and preserves the ring when promoting duplicates.
Malformed settings/policy fail closed. `settings.get.retention.effective` reports
live server limits; stock Ringboard is not a supported ingestion/mutation engine.
Memory-backed files remain subject to OS swapping; existing history is not
retroactively erased. Resource details are in [ADR 0003](adr-0003-daemon-wayland-capture.md).

Privacy controls the managed collector only, not unrelated watchers or direct
Ringboard clients. An ingestion request already sent may commit before pause is
acknowledged. An uncertain reply stops ingestion and is not blindly retried;
exactly-once persistence across crashes is not promised. Headless or unsupported
compositor startup leaves the API available with capture unavailable. Missing
settings use first-start defaults, while malformed settings require repair.

Hyprland targets are revalidated by the compositor when the post-hide shortcut is sent. If the target disappeared, the item remains selected and a copy-only notification is shown. Unsupported compositors remain copy-only. Terminal classes are configurable only in code at this phase and use `Ctrl+Shift+V`; other targets use `Ctrl+V`.

Wayland selection ownership does not expose target-application paste acknowledgement. The API reports `paste-prepared` after the compositor accepts the selection and requests picker hiding, but it does not claim application-level insertion completion.
