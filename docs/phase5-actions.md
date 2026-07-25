# Phase 5 intelligent actions

`clip-daemon` enforces the action matrix; QML only presents the actions returned by product policy.

| Kind | Explicit actions |
|---|---|
| Text, HTML, JSON, color | copy, paste, bounded inline edit |
| Link | copy, paste, bounded inline edit, open validated HTTP(S) URL |
| Image | copy, paste image data (default), paste as a materialized file link, Satty annotate |
| Files | copy, paste, open local file, reveal local file |
| Unknown binary | copy only |

Browsing history never opens a URL or file and performs no preview fetch. URL opening occurs only after an explicit action, parses the complete value in Rust, and allows only HTTP and HTTPS. File actions require a local `file:` URI and recheck existence immediately before launch. Missing files return `entry-not-found`.
File reveal uses the desktop-neutral `org.freedesktop.FileManager1.ShowItems` D-Bus interface. Both image paste modes use the captured paste session: the default restores the original image MIME, while the alternative creates a private image file, restores its `text/uri-list`, hides the picker, and dispatches paste to the original target.

Edit leases are one-use, expire after 60 seconds, and are limited to 256 KiB of valid UTF-8. Commit revalidates the entry revision. Ringboard replacement uses add/swap/remove so the original ring and favorite position are retained before the replacement becomes the current selection. Wipe clears all pending edit leases.

Satty output is decoded and size-checked, replaces the original image in its ring, and is then restored to the clipboard. Cancellation aborts the tracked task. OCR remains an optional later milestone and is not part of phase 5.
