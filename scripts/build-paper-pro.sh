#!/usr/bin/env bash
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
# Git Bash (Windows) rewrites -w /work into C:/Program Files/Git/work without this; no effect elsewhere.
export MSYS_NO_PATHCONV=1
# The native Linux ARM64 image builds on Apple Silicon without a macOS cross linker.
docker run --rm --platform linux/arm64 \
  -v "$repo:/work" -v smart-remarkable-cargo:/usr/local/cargo -w /work \
  rust:1.96-bookworm \
  cargo build --locked --release --target aarch64-unknown-linux-gnu \
  --bin smart_remarkable --bin capture
