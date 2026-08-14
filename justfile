set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

lint:
    cargo clippy --all-targets -- -D warnings

test:
    cargo test

audit:
    cargo audit

check: fmt-check lint test

contract:
    cargo run -- debug protocol-registry

probe:
    cargo run -- probe-ringboard

hardware-acceptance:
    ./scripts/hardware-acceptance.sh check

nix-check:
    nix flake check --show-trace
