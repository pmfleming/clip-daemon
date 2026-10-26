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

This step does not yet expose public status APIs or add socket ingress.
