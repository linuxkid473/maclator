#!/bin/bash
# Build maclator + the GPU bridge and install them into $PREFIX (default ~/bin).
#   TARGET=x86_64-apple-darwin scripts/install.sh   # when building on an Apple Silicon Mac (cross-compile)
set -euo pipefail
cd "$(dirname "$0")/.."
PREFIX=${PREFIX:-$HOME/bin}
TARGET=${TARGET:-}
command -v cargo >/dev/null || { echo "Rust is required: https://rustup.rs"; exit 1; }
if [ -n "$TARGET" ]; then
  rustup target add "$TARGET" >/dev/null 2>&1 || true
  cargo build --release --target "$TARGET" -p maclator
  BIN=target/$TARGET/release/maclator
else
  [ "$(uname -m)" = x86_64 ] || { echo "not an Intel Mac; set TARGET=x86_64-apple-darwin to cross-compile"; exit 1; }
  cargo build --release -p maclator
  BIN=target/release/maclator
fi
gpu/build.sh
mkdir -p "$PREFIX/gpu"
install -m 755 "$BIN" "$PREFIX/maclator"
install -m 755 gpu/out/libmclbridge.dylib gpu/out/libmclmetal.dylib "$PREFIX/gpu/"
for s in gpu/chromium-gpu scripts/vscode-gpu scripts/etcher-gpu; do install -m 755 "$s" "$PREFIX/$(basename "$s")"; done
echo "installed to $PREFIX (make sure it is on PATH). Next: scripts/fetch-sysroot.sh"
