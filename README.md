# clip-daemon

Rust clipboard policy and `clip-api` facade for the Shelllist clipboard surface. Ringboard owns Wayland capture, storage, selection ownership, and low-level paste; this daemon owns the stable UI boundary and product policy.

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

Phase 3 adds copy and compositor-aware paste sessions, terminal/GUI shortcuts after the picker is hidden, image-as-file materialization, Satty annotation with validated PNG return, and two-phase history wipe. Phase 4 adds delete, favorite/current pinning, pause/private mode, native Ringboard retention settings, cancellation, and cache cleanup. Generated files use collision-safe names and private runtime/cache permissions. Ringboard remains the only selection owner, avoiding a second restore/capture pipeline.

See [`docs/phase4-safety.md`](docs/phase4-safety.md) for enforced privacy behavior and explicit Ringboard/Wayland limitations.

Run the local quality review with:

```sh
nix develop --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
```

See [`docs/adr-0001-ringboard-facade.md`](docs/adr-0001-ringboard-facade.md) and [`docs/quality-review.md`](docs/quality-review.md).
