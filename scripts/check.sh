#!/usr/bin/env bash
# Format, lint, and test the whole workspace. Linux counterpart of check.ps1.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
