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

storage-contract:
    cargo run --locked --manifest-path tools/turso-contract/Cargo.toml

backend-regressions:
    cargo test --locked --test turso_storage --test turso_acceptance

live-acceptance:
    cargo build --locked --bin clip-daemon --examples
    cargo run --locked --example turso_acceptance -- --nested

capture-acceptance: live-acceptance

package-acceptance package:
    cargo build --locked --examples
    cargo run --locked --example turso_acceptance -- --nested --package {{quote(package)}}

benchmark-history:
    cargo run --locked --release --example turso_benchmark

contract:
    cargo run --locked -- debug protocol-registry

probe:
    cargo run --locked -- store-status

nix-check:
    ../daemon-framework/tools/local-build check . --show-trace
