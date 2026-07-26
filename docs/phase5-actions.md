# Phase 5 intelligent actions

`clip-daemon` enforces the action matrix; QML only presents the actions returned by product policy.

| Kind | Explicit actions |
|---|---|
| Text, HTML, JSON, color | copy, paste, bounded inline edit |
| Link | copy, paste, bounded inline edit, open validated HTTP(S) URL |
| Image | copy, paste image data (default), paste as a materialized file link, external-editor annotate |
| Files | copy, paste, open local file, reveal local file |
| Unknown binary | copy only |

Browsing history never opens a URL or file and performs no preview fetch. URL opening occurs only after an explicit action, parses the complete value in Rust, and allows only HTTP and HTTPS. File actions require a local `file:` URI and recheck existence immediately before launch. Missing files return `entry-not-found`.
File reveal uses the desktop-neutral `org.freedesktop.FileManager1.ShowItems` D-Bus interface. Both image paste modes use the captured paste session. Raw image entries and single local image-file entries are normalized as semantic images: the default publishes validated image bytes with their detected image MIME, while the alternative publishes `text/uri-list`. The alternative creates a private image file only for raw clipboard images and reuses an existing safe local image file otherwise. Shelllist then hides the picker and the daemon dispatches paste to the original target.

Edit leases are one-use, expire after 60 seconds, and are limited to 256 KiB of valid UTF-8. Commit revalidates the entry revision. Ringboard replacement uses add/swap/remove so the original ring and favorite position are retained before the replacement becomes the current selection. Wipe clears all pending edit leases.

The configured image-editor adapter receives a private staging copy rather than the original file and returns through a separate daemon-owned output path. The adapter contract is application-neutral: the process blocks until editing ends, a valid PNG output means commit, and no output means cancellation. Clipboard publication remains daemon-owned so it can decode and size-check the result, replace the original history entry, and publish it exactly once. The default Satty adapter maps Save, Enter, right-click save, and Copy onto this contract; Save As remains an explicit external export. Cancellation aborts the tracked task. OCR remains an optional later milestone and is not part of phase 5.
