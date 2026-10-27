# Paste handoff lifetime

Normal paste, image-as-file and save-and-paste publish before arming a paste
session. `clipboard.session.hidden` remains a single-use acknowledgement; its
response precedes the delayed compositor dispatch.

After that acknowledgement, a frontend must keep its connection alive while the
picker is hidden. `clipboard.session.wait {session_id}` observes the owned hidden
session and returns `session.state = "finished"` only when its dispatch task has
finished. It does not publish, arm, hide, replay or detach any work. “Finished”
means the dispatch attempt ended, not that the target application consumed the
clipboard. Existing manual-paste notification behavior remains.

Unknown, foreign, unhidden, ended and expired sessions are rejected. End, owner
loss and shutdown still cancel delayed input. Completed sessions remain readable
until their original expiry or owner release, so reopening cannot race the wait.
Lost receipts remain uncertain; never recover by repeating paste or hidden.

Shelllist retains the connection before emitting its hide request, releases that
hold on the wait receipt/error or transport loss, and never applies that receipt
as the active session of a newly opened picker. Both packages must be upgraded.

Regression owners: `session::tests`, the private service-process delivered-paste
case, and Shelllist's `tst_clipboard_recovery.qml` (real Return/Shift+Return input,
hide/deactivate, reopen, disconnect and failed-wait paths). These tests do not
claim live application paste acceptance.
