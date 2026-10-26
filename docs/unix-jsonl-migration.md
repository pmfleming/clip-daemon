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

## Step 3 — direct Unix ingress

`$XDG_RUNTIME_DIR/clip-daemon.sock` uses the shared canonical private-runtime,
same-UID listener. Calls, subscription admission and cancellation go through the
same exported in-process service as D-Bus, never a proxy or second policy engine.
Request IDs, routes and generations use `ClipCorrelation`. The clipboard framing
allowance matches the retained client's 128 MiB escaped-text request limit;
connections and in-flight socket requests are capped at four each, in addition to
shared service admission. Responses retain the shared 32 MiB bound. Output and
pre-ack queues are bounded and delivery loss closes the affected connection.

Receivers are established before subscription acknowledgement. Socket lifetime
owns subscription cancellation and API cleanup, including accepted work finishing
after disconnect. D-Bus monitoring never probes a socket identity or retires a
healthy socket owner on an unrelated bus-owner loss. Shared history/operation
observers are started once per exported service, not once per consumer.

Private real-listener tests compare D-Bus/socket reads and validation, preserve
routes, reject foreign status/cancellation, preserve a healthy peer, reject old
ownership on reconnect, and cancel an operation admitted after disconnect before
acknowledgement (multi-worker runtime). No capture or hardware helpers run.
Validation: 55 library / six integration passes, strict Clippy/formatting.
Frontend opt-in and native Quickshell evidence are not claimed by this step.

## Step 4 — frontend opt-in and native acceptance

`SHELLLIST_CLIP_SOCKET=1` independently selects the shared Unix client. The bridge
remains default; no automatic fallback or effect replay is introduced. Matching
producer/frontend metadata is required on both transports. The clipboard delivery
policy fences old history/detail reads against initial/reset/lag invalidations,
clears sensitive preview caches, and rejects malformed or older watermarks before
publication. Ordinary history refresh also repairs quiet invalidation loss.

Only operations acknowledged to that consumer enter its recovery set. Owned
status reads run at one-second intervals, with one outstanding and a five-second
uncertainty deadline; a timed-out read never starts another read or effect.
Terminal, malformed and unknown outcomes retire the operation locally. Late
observations cannot reopen it or alter a peer's work. Gaps read status rather than
clearing accepted-work guards. Drafts survive recovery; session discontinuity
forgets paste authority and never acknowledges hidden or republishes to repair it.

The feature-gated, normally ignored `native_quickshell_clip_socket_acceptance`
launches actual Quickshell through the production shared client and recovery
policy. Its private real listener/exported service uses an in-memory clipboard,
isolated settings, no service/capture helpers and a read-only dispatch gate. Two
consumers share one owner, read and reconcile a native history invalidation racing
an older response, renew one subscription, destroy one consumer and verify the
surviving peer. Owner disconnect is observed before listener shutdown. The child
has an empty inherited environment, private HOME/runtime and nonexistent buses.
See Shelllist `tests/README.md` for explicit invocation; absent inputs fail.

Payload qualification also passes: a real socket accepts an exact 128 MiB request
and rejects one byte over without executing the fixture deletion; an exact 32 MiB
response (including LF) is accepted, while one byte over retires only its owner.
A genuinely non-reading peer triggers the writer deadline and owned-operation
cleanup without cancelling a healthy peer's work. The stalled test shortens the
write deadline to 100 ms; this is failure-path evidence, not a production latency
measurement. Run the normally ignored `payload_qualification` library tests
explicitly with `-- --ignored`; ordinary release package checks include them.

The native probe additionally sends a 16 MiB all-control-character fixture input
(96 MiB escaped JSON) through a read-only query and receives a near-32 MiB UTF-8
response. Clipboard alone reserves three bytes per UTF-16 unit for up to a 128 MiB
record plus LF, with four calls outstanding. Other daemons retain their existing
write budgets. The producer frame ceiling is unchanged; the larger frontend
reservation fixes an otherwise incompatible generic 4 MiB budget.

This is debug/private framing and read/recovery acceptance, not sustained mixed
load, matched release performance, OS-process crash or installed-session evidence.
No host service or clipboard state was changed.

Step-4 validation: 55 ordinary library passes, five API and one retained-client
integration pass; the two heavy payload tests and the native two-case probe pass
when explicitly invoked. All-target/all-feature strict Clippy and formatting pass.
The frontend reports 429 Qt passes / one existing renderer skip, strict QML lint,
generated-contract and offline producer parity, surface and build-input checks.

## Step 5 — matched release qualification, not rollout

Producer `a017338` and frontend `0b2c848` pass sibling-source snapshot preflight
and Nix `clipDaemonContract`, `qmlTests` and `buildInputs`, without source overrides
or bypasses. The release producer runs 57 library, five API and one retained-client
test, including both heavy framing cases. The packaged frontend runs 429 actual
Qt cases / one existing renderer skip. Source checks, strict Clippy/formatting,
QML lint, generated/offline contract parity and surface/build-input review pass.
The explicit native probe passes both ordinary and large-record cases, separately
from the release package's test suite.

Shelllist's tracked `tests/clipboard-socket-qualification.md` records source
identities, reproduction, scope and remaining gates. The Projects-level migration
report/plan are updated too. All five daemon paths now have scoped opt-ins and
private acceptance, but the wider migration is incomplete. BT/NM lifecycle rows,
sustained mixed load, matched release performance, actual clipboard daemon process
crash/restart, full startup/activation and installed-session acceptance remain.
The bridge remains default; matching producers are required on both transports.
No rollout, deployment, push or live clipboard/hardware operation is authorized
or performed by this qualification stage.
