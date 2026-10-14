#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p integration/build
clang -arch x86_64 -fobjc-arc -O2 -framework AppKit -framework Carbon \
  integration/launcher.m -o integration/build/MaclatorLauncher
clang -arch arm64 -mmacosx-version-min=26.0 -fobjc-arc -fblocks -O2 -dynamiclib \
  -framework Foundation integration/app_icon_shim.m \
  -install_name @rpath/libmclappicon.dylib -o integration/build/libmclappicon.dylib
codesign --force --sign - integration/build/libmclappicon.dylib
cargo build --release -p maclator -j 2 --target-dir integration/runtime-target
cp integration/build/libmclappicon.dylib integration/runtime-target/release/libmclappicon.dylib
