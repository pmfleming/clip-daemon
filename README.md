# clip-daemon

Rust Wayland clipboard collector and `clip-api` facade for Shelllist. The daemon owns regular-selection capture, publication, paste targeting, and product policy. The policy-enabled Ringboard server owns persistent history, favorites, retention, and atomic mutations.

The package ships two services: daemon and server. The external watcher is retired.
See [ADR 0003](docs/adr-0003-daemon-wayland-capture.md), the
[migration record](docs/capture-migration-progress.md), and
[qualification results and remaining gates](docs/capture-qualification.md).
Production cutover remains a deliberate, separate operation.

## Installation

See [`docs/installation.md`](docs/installation.md) for a standalone Nix build,
packaged daemon/server units, first-start privacy ordering, and isolated package
verification. Keep `daemon-framework` beside this checkout: all five daemons
consume that current worktree, never a private framework pin or vendored copy.

## Local Rust environment

```sh
direnv allow
# or
python3 ../daemon-framework/tools/local-build.py develop .
just check                 # locked tests, Clippy, unused dependencies, RustSec
just quality               # full RQLens evidence and verification
just live-acceptance       # disposable nested desktop; never wipes normal history
just backend-regressions   # real-server storage/concurrency/admission regressions
just capture-acceptance    # collector privacy/recovery, edits, and 64 MiB boundary
just benchmark-history     # synthetic release projection comparison
just hardware-acceptance   # remaining manual hardware gates
```

The tested minimum toolchain is Rust **1.95.0**, pinned in `rust-toolchain.toml` and supplied by the development flake. The SDK/core are locked to **0.16.2** to match the reviewed Ringboard protocol. Safe history mutations require the patched `.#ringboard` package (included in the development shell); stock servers are detected before mutation and rejected without changing history. See `packaging/ringboard-policy/README.md` for the negotiated extension and its limits. These upstream crates still require `core_io_borrowed_buf`; the Nix environment scopes `RUSTC_BOOTSTRAP` to `clipboard_history_core,clipboard_history_client_sdk`, not the daemon or all dependencies. Outside Nix, use the same scoped environment when building on 1.95.0. This is an explicit upstream compatibility exception, not a claim of an exception-free stable dependency graph.

The default live suite has 15 checks. Supplying the real Shelllist package paths
adds two layer-shell paste checks (17 total); see
[`docs/qualification-results.md`](docs/qualification-results.md). Physical
keyboard/login qualification remains a separate manual gate.

`just check` requires network access to refresh RustSec advisories (or an already usable local database). The known unmaintained transitive `paste` advisory is reported, not suppressed. Do not blindly update Ringboard's broad internal core range to 0.17; it is protocol/API-incompatible. See `docs/follow-up-validation.md` for gate results and `docs/history-benchmark.md` for benchmark scope and results.

## Commands

```sh
clip-daemon configure-engine # before starting Ringboard; validate/apply native configuration
clip-daemon daemon
clip-daemon client
clip-daemon publish --mime image/png < image.png
clip-daemon probe-ringboard
clip-daemon debug protocol-registry
clip-daemon debug contract-fixture
nix run .#qualify
```

`client` accepts JSONL calls such as:

```json
{"op":"call","id":"q1","method":"clipboard.history.query","params":{"query":"","generation":1,"limit":100}}
```

The daemon supports bounded history queries with complete-text search (Unicode lowercase matching, queries up to 4096 bytes), semantic details, private image thumbnails, exact-MIME restoration through Ringboard, opaque entry IDs, structured errors, D-Bus/JSONL transport, and the checked `clip-api` v1 registry. History metadata is polled only while a frontend subscription exists. `clipboard.settings.get` separates desired/effective capture and retention state; unverified privacy is never asserted. Existing native retention counts are adopted on first use, while a fresh setup receives the documented defaults.

Phase 3 adds copy and compositor-aware paste sessions, terminal/GUI shortcuts after the picker is hidden, image-as-file materialization, external image annotation with validated PNG return, and two-phase history wipe. Phase 4 adds delete, favorite/current pinning, pause/private mode, native Ringboard retention settings, cancellation, and cache cleanup. Phase 5 adds bounded inline editing, explicit validated URL/file launch actions, a daemon-enforced type/action matrix, and position-preserving text/image replacement. Generated files use collision-safe names, private permissions, and a persistent ownership registry; unreferenced daemon-owned files are pruned without touching unrelated files. Equivalent Ringboard echoes of generated file URIs and completed annotations are collapsed by default in the API projection and can be retained with the `collapse_self_echoes` setting. Raw clipboard images and single safe local image-file entries use the same `ResolvedContent` policy: MIME aliases, summaries, details, thumbnails, normal image publication, and file-URI publication all resolve through one abstraction. `clip-daemon` captures and publishes Wayland selections independently; pausing automatic capture does not disable explicit publication or edits. Resuming discards the current-selection bootstrap snapshot, so content copied during a private interval is not replayed. Copy again to capture it.

`publish` reads bounded content from stdin and sends it over D-Bus to the running daemon. The daemon enforces the configured entry-size limit, validates the MIME type, and remains the Wayland selection owner. Valid UTF-8 plain text retains its exact offer and also exposes standard text aliases for GTK and other desktop consumers; binary/image/file-list offers do not gain text aliases. This supports short-lived producers without `wl-copy`; for example, standalone Satty can use `copy-command = "clip-daemon publish --mime image/png"`.

The default image-editor adapter uses Satty. The Nix package and development shell use a patched Satty (`.#imageEditor`) with toolbars above and below the canvas rather than overlaid on the image, so fit-to-window keeps the entire image visible. This changes only the editor layout, not the saved image dimensions. The annotation pipeline itself is editor-neutral: an editor receives private `{input}` and `{output}` paths, blocks until it finishes, and either writes a PNG to `{output}` or leaves it absent to cancel. A different editor can be selected with a shell-free JSON argv template:

```sh
export CLIP_DAEMON_IMAGE_EDITOR_COMMAND='["image-tool","--input","{input}","--output","{output}"]'
```

Both placeholders must be separate arguments. The custom editor must not publish the Wayland clipboard itself; `clip-daemon` validates and publishes the returned image.

## Yazi copied files

Yazi keeps its normal file yank state internally. The bundled `yank-to-clip-daemon.yazi` plugin mirrors each non-empty yank to `clipboard.selection.publishFiles`, preserving copy/cut mode and multi-file selections. The daemon validates up to 100 absolute local paths, encodes them as file URIs, and publishes both `x-special/gnome-copied-files` and `text/uri-list` in one Wayland selection. No `wl-copy` process or additional runtime dependency is required.

When packaged, the plugin is available at `$out/share/yazi/plugins/yank-to-clip-daemon.yazi`. Configure it through Yazi or Home Manager and call its `setup` function. Empty unyank events intentionally leave the current system clipboard unchanged.

See [`docs/phase4-safety.md`](docs/phase4-safety.md) for enforced privacy behavior and explicit Ringboard/Wayland limitations, and [`docs/phase5-actions.md`](docs/phase5-actions.md) for intelligent-action policy.

Run the local quality review with:

```sh
python3 ../daemon-framework/tools/local-build.py develop . --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
```

See [`docs/adr-0001-ringboard-facade.md`](docs/adr-0001-ringboard-facade.md), [`docs/adr-0002-wayland-selection-ownership.md`](docs/adr-0002-wayland-selection-ownership.md), and [`docs/quality-review.md`](docs/quality-review.md).
