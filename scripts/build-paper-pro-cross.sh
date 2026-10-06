#!/usr/bin/env bash
# Cross-compile the Paper Pro (aarch64) binaries from an x86_64 host: Windows
# (Docker Desktop has no arm64 emulation by default, so build-paper-pro.sh fails
# with "exec format error"), Intel Macs, and Linux. Same toolchain image, same
# --locked dependency set and binaries as build-paper-pro.sh; only the linker
# differs. Added for the Windows evaluation; build-paper-pro.sh is unchanged.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
# Git Bash would otherwise rewrite /work and the volume path into C:/Program Files/Git/...
export MSYS_NO_PATHCONV=1
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) repo=$(cygpath -m "$repo") ;;
esac
docker run --rm --platform linux/amd64 \
  -v "$repo:/work" -v smart-remarkable-cargo-amd64:/usr/local/cargo/registry -w /work \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  -e CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  rust:1.96-bookworm \
  sh -c 'apt-get update -qq && apt-get install -y -qq gcc-aarch64-linux-gnu libc6-dev-arm64-cross >/dev/null \
    && rustup target add aarch64-unknown-linux-gnu \
    && cargo build --locked --release --target aarch64-unknown-linux-gnu --bin smart_remarkable --bin capture'
