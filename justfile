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

check: fmt-check lint test

contract:
    cargo run -- debug protocol-registry

probe:
    cargo run -- probe-ringboard

nix-check:
    nix flake check --show-trace
