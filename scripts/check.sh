#!/usr/bin/env sh
set -eu

cargo fmt --check
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
if cargo nextest --version >/dev/null 2>&1; then
  cargo nextest run --all-features
fi
