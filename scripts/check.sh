#!/usr/bin/env sh
set -eu

cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo nextest run --all-features
