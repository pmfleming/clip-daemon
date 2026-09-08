set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets --all-features --locked -- -D warnings

test:
    cargo test --all-targets --all-features --locked

audit:
    cargo audit

deps:
    cargo machete --with-metadata

check: fmt-check lint test deps audit

quality:
    ../rust-quality-lens/target/debug/rqlens measure all --config rqlens.toml
    ../rust-quality-lens/target/debug/rqlens verify --config rqlens.toml
    ../rust-quality-lens/target/debug/rqlens check --config rqlens.toml --fail-on partial --fail-on test-failure --fail-on practice-failure

live-acceptance:
    cargo build --locked
    python3 scripts/isolated-acceptance.py

benchmark-history:
    python3 scripts/benchmark-history.py

contract:
    cargo run -- debug protocol-registry

probe:
    cargo run -- probe-ringboard

hardware-acceptance:
    ./scripts/hardware-acceptance.sh check

nix-check:
    nix flake check --show-trace
