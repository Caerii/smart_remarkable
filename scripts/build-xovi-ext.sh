#!/usr/bin/env bash
# Build a XOVI extension (default: xovi-ext/llmbutton) for the Paper Pro (aarch64).
#
#   scripts/build-xovi-ext.sh [extension-dir]
#
# Output: target/xovi/<name>.so. Copy it to /home/root/xovi/extensions.d/ on
# the tablet and restart xochitl under XOVI.
#
# xovi.c/xovi.h are xovigen output and are committed. After editing the
# .xovi manifest, regenerate them by pointing XOVIGEN at xovigen.py from
# https://github.com/asivery/xovi (util/xovigen.py):
#
#   XOVIGEN=~/src/xovi/util/xovigen.py scripts/build-xovi-ext.sh
#
# Uses aarch64-linux-gnu-gcc when installed, otherwise a Debian container
# with the cross compiler (no emulation needed).
set -euo pipefail

repo=$(cd "$(dirname "$0")/.." && pwd)
ext_dir=${1:-xovi-ext/llmbutton}
ext_dir=${ext_dir%/}
name=$(basename "$ext_dir")
manifest="$ext_dir/$name.xovi"
out_dir=target/xovi
cflags="-std=gnu11 -O2 -fPIC -shared -Wall"
libs="-ldl -lpthread"

cd "$repo"
test -f "$ext_dir/main.c" || { echo "No main.c in $ext_dir" >&2; exit 1; }

if [ -n "${XOVIGEN:-}" ]; then
    test -f "$manifest" || { echo "No manifest $manifest" >&2; exit 1; }
    (cd "$ext_dir" && python3 "$XOVIGEN" -o xovi.c -H xovi.h "$name.xovi")
fi
test -f "$ext_dir/xovi.c" || { echo "No xovi.c in $ext_dir; set XOVIGEN to generate it" >&2; exit 1; }

mkdir -p "$out_dir"
compile="aarch64-linux-gnu-gcc $cflags -o $out_dir/$name.so $ext_dir/main.c $ext_dir/xovi.c $libs"

if command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
    $compile
else
    # Docker Desktop on Windows wants a native path for the bind mount.
    mount=$( (cd "$repo" && pwd -W 2>/dev/null) || echo "$repo")
    MSYS_NO_PATHCONV=1 docker run --rm --platform linux/amd64 \
        -v "$mount:/work" -w /work debian:bookworm-slim \
        sh -c "apt-get update -qq >/dev/null && apt-get install -y -qq gcc-aarch64-linux-gnu libc6-dev-arm64-cross >/dev/null && $compile"
fi

echo "Built $out_dir/$name.so"
