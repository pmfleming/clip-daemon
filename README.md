# clip-daemon

Rust clipboard policy and `clip-api` facade for the Shelllist clipboard surface. Ringboard owns capture, persistent history, favorites, and retention; this daemon owns the stable UI boundary, Wayland selection publication, paste targeting, and product policy.

## Local Rust environment

```sh
direnv allow
# or
nix develop
just check
```

The flake provides Rust/Cargo tooling and the pinned Nixpkgs Ringboard package. The SDK is pinned to 0.16.2 to match the reviewed Ringboard protocol.

## Commands

```sh
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

The daemon supports bounded history queries, semantic details, private image thumbnails, exact-MIME restoration through Ringboard, opaque entry IDs, structured errors, D-Bus/JSONL transport, and the checked `clip-api` v1 registry. History metadata is polled only while a frontend subscription exists.

Phase 3 adds copy and compositor-aware paste sessions, terminal/GUI shortcuts after the picker is hidden, image-as-file materialization, external image annotation with validated PNG return, and two-phase history wipe. Phase 4 adds delete, favorite/current pinning, pause/private mode, native Ringboard retention settings, cancellation, and cache cleanup. Phase 5 adds bounded inline editing, explicit validated URL/file launch actions, a daemon-enforced type/action matrix, and position-preserving text/image replacement. Generated files use collision-safe names, private permissions, and a persistent ownership registry; unreferenced daemon-owned files are pruned without touching unrelated files. Equivalent Ringboard echoes of generated file URIs and completed annotations are collapsed by default in the API projection and can be retained with the `collapse_self_echoes` setting. Raw clipboard images and single safe local image-file entries use the same `ResolvedContent` policy: MIME aliases, summaries, details, thumbnails, normal image publication, and file-URI publication all resolve through one abstraction. `clip-daemon` publishes exact-MIME Wayland selections directly while Ringboard remains the sole capture/history engine.

`publish` reads bounded content from stdin and sends it over D-Bus to the running daemon. The daemon enforces the configured entry-size limit, validates the MIME type, and remains the Wayland selection owner. This supports short-lived producers without `wl-copy`; for example, standalone Satty can use `copy-command = "clip-daemon publish --mime image/png"`.

The default image-editor adapter uses Satty. The annotation pipeline itself is editor-neutral: an editor receives private `{input}` and `{output}` paths, blocks until it finishes, and either writes a PNG to `{output}` or leaves it absent to cancel. A different editor can be selected with a shell-free JSON argv template:

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
nix develop --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
```

See [`docs/adr-0001-ringboard-facade.md`](docs/adr-0001-ringboard-facade.md), [`docs/adr-0002-wayland-selection-ownership.md`](docs/adr-0002-wayland-selection-ownership.md), and [`docs/quality-review.md`](docs/quality-review.md).
