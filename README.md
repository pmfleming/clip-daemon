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

Phase 3 adds copy and compositor-aware paste sessions, terminal/GUI shortcuts after the picker is hidden, image-as-file materialization, Satty annotation with validated PNG return, and two-phase history wipe. Phase 4 adds delete, favorite/current pinning, pause/private mode, native Ringboard retention settings, cancellation, and cache cleanup. Phase 5 adds bounded inline editing, explicit validated URL/file launch actions, a daemon-enforced type/action matrix, and position-preserving text/image replacement. Generated files use collision-safe names and private runtime/cache permissions. Raw clipboard images and single safe local image-file entries use the same policy: normal paste publishes image data and the alternate action publishes a file URI. `clip-daemon` publishes exact-MIME Wayland selections directly while Ringboard remains the sole capture/history engine.

See [`docs/phase4-safety.md`](docs/phase4-safety.md) for enforced privacy behavior and explicit Ringboard/Wayland limitations, and [`docs/phase5-actions.md`](docs/phase5-actions.md) for intelligent-action policy.

Run the local quality review with:

```sh
nix develop --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
```

See [`docs/adr-0001-ringboard-facade.md`](docs/adr-0001-ringboard-facade.md), [`docs/adr-0002-wayland-selection-ownership.md`](docs/adr-0002-wayland-selection-ownership.md), and [`docs/quality-review.md`](docs/quality-review.md).
