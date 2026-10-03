#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

rustc_version="$(rustc --version)"
if [[ ! "$rustc_version" =~ ^rustc\ 1\.98\. ]]; then
  echo "Expected Rust 1.98.x, found: $rustc_version" >&2
  exit 1
fi

cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings

if command -v xvfb-run >/dev/null 2>&1; then
  GAME_LARPER_REQUIRE_X11=1 xvfb-run --auto-servernum cargo test --workspace --no-fail-fast
else
  cargo test --workspace --no-fail-fast
fi

cargo build --workspace --release

out="artifacts/release/linux-x64"
archive="artifacts/release/GameLarper-linux-x64.tar.gz"

rm -rf "$out"
rm -f "$archive"
mkdir -p "$out"

install -m 0755 target/release/game-larper "$out/game-larper"
install -m 0755 target/release/game-larper-runner "$out/game-larper-runner"
install -m 0644 LICENSE "$out/LICENSE"

if [[ ! -x "$out/game-larper-runner" ]]; then
  echo "Runner missing from the release folder." >&2
  exit 1
fi

tar -C "$out" -czf "$archive" .
echo "Release ready: $archive"
