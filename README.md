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

Phase 1 supports bounded read-only history queries, semantic details, image thumbnails in a private cache, exact-MIME-first classification, daemon-parsed file metadata, opaque entry IDs, structured errors, D-Bus/JSONL transport, and the checked `clip-api` v1 registry. History metadata is polled only while a frontend subscription exists; changes publish monotonic reset/current events. Mutations and universal paste remain reserved in the contract and return `not-implemented` until qualification and safety work is complete.

Run the local quality review with:

```sh
nix develop --command ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
```

See [`docs/adr-0001-ringboard-facade.md`](docs/adr-0001-ringboard-facade.md) and [`docs/quality-review.md`](docs/quality-review.md).
