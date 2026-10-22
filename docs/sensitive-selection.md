# Sensitive clipboard publication

`clipboard.selection.publishSensitive` accepts `{text, expiry_seconds}`: UTF-8
text of 1–256 bytes, and either 0 (Never) or 10–300 seconds. It acknowledges only
after the Wayland data-control ownership sync. Unsupported publishers fail closed;
there is no fallback to ordinary text publication.

Unlike the ordinary wl-clipboard publisher, the sensitive source serves bytes
from memory, not temporary files. It offers text aliases and the
`x-kde-passwordManagerHint` / `application/x-shelllist-sensitive` markers. The
native capture policy excludes the entire offer regardless of MIME order. No
history insertion, deduplication or backend history publication occurs.

A monotonic deadline destroys only the source's Wayland connection. It never
issues `set_selection(None)`, compares clipboard text or clears another owner's
selection. Replacement cancels ownership normally; shutdown ends all sources.
At most 16 sensitive owners, 16 seats per owner and 256 bytes per payload are
admitted. Initialization is bounded; transfers cannot block the expiry loop.
Neither requests nor payloads are logged. External managers that ignore privacy
markers are outside this guarantee; this is not a guarantee of memory zeroization
or erasure of content persisted elsewhere (such as notification history).

`selection::secret::protocol_tests` uses an isolated Wayland server to exercise
actual source lifetime/expiry and verify that a newer selection survives. Capture
policy tests verify marker exclusion, including oversized MIME offers. No test
uses the user's clipboard or compositor.
