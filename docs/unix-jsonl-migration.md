# Clipboard Unix JSONL migration stages

The bridge remains the frontend default. This work does not authorize deployment,
live service restarts, real clipboard/hardware probes or effect replay.

## Step 1 — asynchronous operation ownership

Backend operation snapshots retain up to 256 terminal results for five minutes,
atomically with live-job removal. The API registers ownership even when completion
precedes acknowledgement, and treats backend broadcasts only as invalidations of
those snapshots. One daemon observer publishes owner-filtered API receipts; raw
backend events are no longer delivered to subscriptions. Delayed start records
cannot overwrite terminal observations. Lag reconciles through backend status.

Admission reserves space before asynchronous dispatch (64 global / 16 per owner),
including requests not yet acknowledged. Reservations are released on abandoned
API futures. The API retains at most 256 completed records for five minutes,
without evicting live work. Foreign cancellation is rejected. Disconnect cleanup
cancels only the owner's editing work; committing work keeps its real outcome.
Post-dispatch cleanup also catches late acceptance. Backend cancellation draining
continues if its waiter disappears and remains visible to concurrent cleanup.

The screenshot CLI now subscribes before launching and keeps its D-Bus connection
alive until a terminal result. This preserves the interactive screenshot instead
of accidentally cancelling it at acknowledgement when connection-owned cleanup
runs. CLI exit no longer reports success merely for acceptance; loss/gaps fail
with an explicit unknown outcome and never retry the screenshot. Killing the CLI
releases its owner and requests cancellation, not rollback.

Step 1 does not expose public status APIs or add socket ingress.

## Step 2 — read-only recovery and ordering

`clipboard.operation.status {operation_id}` returns only an owned asynchronous
operation. Responses and events carry `operation._publication {epoch, revision}`;
revision is per operation, JS-safe and atomic with the observed state. A status
read never republishes content, launches an editor or retries capture. Quiet
terminal loss converges from backend retention even without any event observer.
Missing, foreign, expired or lost-before-ack identities return `operation-unknown`.
Retention is bounded, not a durable outcome journal. The CLI polls this read at
one-second intervals with one bounded request outstanding to repair quiet loss.

Interactive capture uses the same operation status. Synchronous capture and paste
publication without a delivered receipt remain explicitly uncertain. Capture
settings recover configuration only. There is deliberately no API that claims a
lost hidden-session acknowledgement proves input injection succeeded. A frontend
must discard that authority on loss and never repeat hidden/paste for recovery.

History queries are sealed by equal content tokens before/after the read, sharing
a serialization barrier with invalidation observations. A changed token rejects
the query as `stale-history`. Queries, explicit revision reads and subscription
baselines carry `_history {epoch, revision}`; history content identities themselves
are strings compared only for equality, never delivery counters. Publication
revision advances for observed changes/recovery, fails closed at the JS-safe
limit, and resets only with a new producer lifetime. Initial and ordinary resets
carry watermarks; lag/unavailability invalidates outstanding frontend reads until
a fresh authoritative read, rather than inventing missing history. Consumers must
reject older metadata, including responses captured before newer invalidations.

Producer recovery tests cover owner isolation, lost final events, terminal expiry,
read-only repeated status, decreasing content tokens and mutation during a blocked
history query. Validation: 53 library / six integration passes, strict Clippy/fmt;
producer fixture export, regenerated frontend bindings, offline contract parity
and QML lint pass. Frontend consumption and socket acceptance are later steps.
