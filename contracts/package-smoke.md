# Package smoke: wiring versus runtime acceptance

`python3 scripts/package-smoke.py PACKAGE --wiring-only` reads rendered package
units/activation files and selected executable paths. It executes **no** package
binary, Ringboard, helper, user manager or bus. The activation and daemon unit
must both name `PACKAGE/bin/clip-daemon daemon`; the engine pre-start must use
that package's `configure-engine`. The two-unit relationship, retired collector
exclusion, notify engine and facade independence remain checked. This is a
static wiring check, not a systemd graph/activation/readiness qualification.
Inherited INI `[DEFAULT]` directives are rejected: they cannot substitute for an
actual packaged systemd/D-Bus directive.

The historical positional invocation (including PREVIOUS rollback) exits **2**
with an explicit NOT RUN gate. `_CLIP_PACKAGE_SMOKE` no longer bypasses isolation.
There is no CLI option to run the real main via this harness. Real main constructs
`RingboardBackend`, a capture `Controller`, and initializes native settings,
retention and capture policy after bus-name acquisition. `Controller::new` itself
is inert, but the normal `ApiService::initialize` path synchronizes engine limits
and reconciles capture; `settings/services.rs` may invoke user `systemctl`, and
capture reconciliation may start native workers. This is not an inert exported
service fixture. No production API/backend test switch is added here. Historical
private-files/invalid-policy/engine-protocol/rollback results are not fresh passes.

## Synthetic activation boundary

The private harness in `scripts/package-smoke.py` is exercised only by
`tests/test_package_smoke.py` with `test_support/package_smoke_service.py`.
It is not a producer-policy implementation or a fake packaged-main pass.

- Tools and synthetic entrypoint are explicit absolute executable paths. Child
  commands never resolve via inherited PATH. Preserve a selected multicall tool's
  invocation basename (for example `timeout`); hash its resolved binary separately.
  Arguments are tokens, not a shell.
- HOME, config/data/state/cache/runtime, config/data search directories, TMPDIR
  and an empty helper PATH are private, mode 0700. Environment is an allowlist;
  host buses, compositor FDs, loader/Python injection, notification/listening FDs,
  Ringboard settings and the old recursion marker are not inherited. System bus
  and Wayland endpoints explicitly name nonexistent private paths.
- A generated `dbus-daemon --nofork` configuration loads **one** explicit private
  service directory, no standard/host service directories, includes, service
  helper or systemd activation. The registered service is the deliberately inert
  fixture, not a copied packaged activation file. Enumerated activatable names
  must be exactly the bus and the synthetic clipboard service; no prior owner.
- Activation is one explicit bounded attempt. Calls use an explicit private bus
  address, `--auto-start=no` and the captured **unique owner**, never a replaceable
  well-known destination. Interface is `org.laufan.ClipDaemon2`; no v1 fallback.
  Owner loss/replacement cannot activate another process or redirect a read.
- The only domain-shaped probe is `clipboard.settings.get` with `{}`. Its fixture
  returns an explicit synthetic marker, not privacy/readiness or admitted-service
  evidence. There are no mutations, publication, paste, capture or real data.
- Commands have time and output bounds (3s / 64 KiB by default). EOF is not
  process completion: expiry while waiting after EOF reports the same redacted
  `SmokeError` deadline failure as expiry while reading. Private service
  name acquisition is limited to 2s. Bus startup is bounded. An explicitly
  selected coreutils `timeout` bounds the bus to 15s (KILL after 1s) even if the
  Python runner dies; the fixture has an independent 10s lifetime. Spawned process groups receive TERM then KILL and are
  waited before removing private data, including failed startup and exceptions.
  The leader remains unreaped until group signalling finishes: non-reaping
  `waitid(..., WNOWAIT)` observes completion/startup failure under the deadline.
  Cleanup is idempotent and rejects already-reaped identities, including external
  `waitpid` misuse. Callers must not independently reap the owned `Popen`.
  This supports inspected tools/fixtures that retain their group, not arbitrary
  detached-process containment. No signal targets a PID discovered through D-Bus. Native regression verifies
  the activated fixture belongs to the owned bus group and leaves no live child.

The fixture uses only Gio/GLib on its explicit private connection and stdlib
metadata/audit files. It rejects non-smoke runtime directories before connecting;
its address must be exactly that runtime's bus path, with at most dbus-daemon's
32-hex GUID suffix (never an alternate address or extra transport option). It imports no daemon/Ringboard backend and opens no default
session/system bus, compositor, clipboard, user catalog or helpers. Audit files
contain only generated fixture environment/metadata, never inherited secrets.

## Reproduction

From the producer in a matching sibling-source development shell:

```sh
python3 -B -m unittest discover -s tests -p test_package_smoke.py -v
CLIP_PACKAGE_NATIVE=1 python3 -B -m unittest discover -s tests -p test_package_smoke.py -v
# Put generated bytecode in a private artifact directory, not the source tree:
PYTHONPYCACHEPREFIX="$PRIVATE_ARTIFACTS/pycache" python3 -m py_compile \
  scripts/package-smoke.py tests/test_package_smoke.py test_support/package_smoke_service.py
```

Native mode requires the matching shell's Python with PyGObject/Gio, dbus-daemon
and explicitly resolved busctl/coreutils timeout. Missing requirements fail; native tests are
otherwise deliberately skipped. Source-contract coverage compares the selected
interface against both current Rust export declarations. Static package fixtures
render the tracked unit templates with inert executables; they do not establish
that an actual built package contains those same definitions.

Use the run's shared `check.sh timeout ...` wrapper, family local-build preflight
and matching development shell for heavy/native checks. Stage new source inputs
with `git add -N` before capture. No source overrides, dependency installations,
lock/unit/flake edits, stale producer binaries or real service execution.

## Retained gates / integration requests

1. Coordinator-owned `checks.<system>.packageSmoke` now runs the construction and
   explicit private native cases with declared Python/Gio, D-Bus and busctl inputs,
   plus static wiring inspection of the real built package. Development checks also
   compile Python syntax. The Rust-only package check does not discover Python
   tests; this separate check does. It never starts real main/engine or rollback.
2. Safe real packaged-main and pinned-engine cold start, configuration failure,
   persisted-state privacy, engine version negotiation, upgrades and rollback
   still require an authorized producer-safe fixture/acceptance environment. Do
   not substitute this inert service or add a production test-backend switch.
3. Effective systemd composition/activation, manager graph, selected startup
   strategy and frontend readiness remain coordinator gates. Bridge default,
   external D-Bus/retained clients, and all existing domain guards are unchanged.
4. Serial matched-release measurements remain separate. Once integration freezes
   sources and the safe real adapter exists, reserve an exclusive shared-lock
   window, capture the family with local-build, build matching release packages,
   record source/store/ELF/helper/engine hashes, fixture parameters and complete
   process trees, then run matched bridge/socket samples serially. Report p50/p99
   and RSS/PSS/private memory or unavailable metrics. This synthetic correctness
   fixture must not be used to claim daemon/desktop performance.

## Worker quality checkpoint (historical)

Rust/QML Quality Lenses do not certify Python. No Rust/QML/JS/TS/shell/Nix source
changes are part of this tranche. The quality continuation reviewed the full
original-baseline diff (including this contract, tests, fixture and frontend
implementation evidence), not only its newest commit. No seeds or generated
companions are omitted. No Lens CLI/extraction helper was run or built; the
Rust/QML tools and their evidence/testing policies were read for applicability.
No applicable existing Python linter/formatter configuration was found, and broad
Rust audit/coverage/live recipes were not used as Python evidence.

At clean implementation heads the existing private suite passed 20 tests, but two
bounded inert probes reproduced introduced gaps: inherited INI defaults could
hide a missing actual unit command, and EOF followed by child wait expiry raised
raw `subprocess.TimeoutExpired`. The shared parser now rejects inherited defaults;
the bounded runner normalizes both expiry paths without exposing child argv.
One focused wiring regression and one extension to the existing deadline owner
cover these distinct risks. No existing test, safety guard or domain policy was
removed, no thresholds/exclusions changed, and no mechanical splitting was used.

Comparable post-refactor checks pass Python syntax, all 21 construction/private
native tests (zero skips), and both regression probes. Source/tool hashes, exact
commands, input identities, BEFORE/AFTER observations and the clean-head replay
are retained under the task's `quality/` artifacts and clipboard-specific quality
evidence. This is **partial unsupported-language quality evidence**, not a Lens
pass or universal semantic/complexity certification. Gio's existing object
registration deprecation remains recorded; actual package-main/session/engine,
upgrade/rollback and performance remain separate. Nix wiring was still open at
that worker handoff; the following coordinator checkpoint closes only the isolated
CI/static-wiring request.

## Coordinator lifetime quality checkpoint — 2026-10-27

Implementation integration `1777c03` adds the reviewed worker tree and isolated
Nix check. All fourteen combined project gates passed before this separate quality
refactor. Manual Python review then identified a group-identity hazard: normal
command completion and startup polling could reap the leader before cleanup
signalled its numeric process-group ID. An idempotence guard alone is insufficient.

A safe BEFORE witness used only inert Python children and mocked **every** group
signal: cleanup requested TERM/KILL with an already-reaped leader; explicit external
reaping was not rejected. The two new regression cases also fail against the exact
archived implementation helper (three assertions, including Popen/raw-waitpid
subcases). These are real behavior regressions with simulated signals, not actual
PID-reuse experiments or signals to foreign processes.

The refactor preserves the leader identity with non-reaping exit observation,
uses the same deadline after EOF, checks parent-child ownership before cleanup,
signals before final wait, and makes repeated close a no-op. Externally reaped
identities fail closed without signalling. The AFTER witness observes an unreaped
leader during normal cleanup and zero signal requests for the rejected identity.
Native owner-loss tests use the same non-reaping observer. No activation retry,
command/output limit relaxation, fixture expansion or domain effect was added.

Python 3.13.13 syntax and **23 construction/private native tests, zero skips** pass
in development and the matching Nix `packageSmoke` check. Static real-package
wiring passes; its dependency rebuild passes 59 Rust library + six integration
tests. Real packaged-main/engine/rollback remains explicitly NOT RUN. The combined
frontend still passes 522 Qt tests with one existing renderer skip. Source seals
and process quiescence were checked before adding documentation.

Evidence: `shelllist-orchestration/20261026T232107Z/integration-review/remaining/`
`coordinator-quality/{before,after}/`, the safe witness/regression drivers, exact
logs and source captures. Python/Nix review remains unsupported by Rust/QML Lens;
actual syntax, execution, Nix parsing/build and manual lifetime review are scoped
evidence, not universal semantic or process-tree containment certification.
