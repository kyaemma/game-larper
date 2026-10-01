#!/usr/bin/env bash
# Format, lint, and test the whole workspace: the Linux counterpart of check.ps1.
# The X11 runner test runs when $DISPLAY reaches an X server (CI uses xvfb-run).
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
