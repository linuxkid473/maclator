#!/bin/bash
# Download the arm64e dyld shared cache of an Apple Silicon macOS release (needs `ipsw`).
#   scripts/fetch-sysroot.sh [VERSION] [DEVICE]      defaults: 26.6.2 Mac14,2
# Only the shared cache is extracted from Apple's IPSW (streamed, ~5.5 GB); nothing is redistributed.
set -euo pipefail
VERSION=${1:-26.6.2}
DEVICE=${2:-Mac14,2}
IPSW=$(command -v ipsw || true)
[ -z "$IPSW" ] && [ -x "$HOME/ipsw-tool/ipsw" ] && IPSW=$HOME/ipsw-tool/ipsw
[ -n "$IPSW" ] || { echo "ipsw not found. Install it: brew install blacktop/tap/ipsw  (or https://github.com/blacktop/ipsw/releases)"; exit 1; }
TMP=$(mktemp -d "${TMPDIR:-/tmp}/maclator-sysroot.XXXXXX")
"$IPSW" download ipsw --macos --version "$VERSION" --dyld --dyld-arch arm64e --extract-device "$DEVICE" --confirm --output "$TMP"
CACHE=$(find "$TMP" -name dyld_shared_cache_arm64e -type f | head -1)
[ -n "$CACHE" ] || { echo "no dyld_shared_cache_arm64e found under $TMP"; exit 1; }
SRC=$(dirname "$CACHE")
BUILD=$(basename "$(dirname "$SRC")")   # ipsw names the parent folder after the build; fall back to the version
DEST=$HOME/maclator-sysroot/${BUILD:-$VERSION}__$DEVICE
mkdir -p "$(dirname "$DEST")"
rm -rf "$DEST"
mv "$SRC" "$DEST"
rm -rf "$TMP"
echo "sysroot: $DEST"
ls "$DEST" | head -3
