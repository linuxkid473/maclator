#!/bin/bash
# Parent-only serial build. No Cargo/shared runtime module or deployment.
set -eu
EO_ROOT="$(cd "$(dirname "$0")" && pwd)"
EO_NATIVE="$EO_ROOT/../native-fault"
EO_BUILD="$EO_ROOT/build"
EO_RUSTC="${RA_RUSTC:-rustc}"
python3 "$EO_ROOT/source_guard.py" --verify
mkdir -p "$EO_BUILD"
"$EO_RUSTC" -vV > "$EO_BUILD/rustc-version.txt"
/usr/bin/clang --version > "$EO_BUILD/clang-version.txt"
/usr/bin/clang -arch x86_64 -c "$EO_NATIVE/native_fault.S" -o "$EO_BUILD/native_fault_asm.o"
EO_CASES=(baseline-observe natural-early prior-custom-segv prior-custom-bus prior-ignore-segv prior-ignore-bus late-custom late-owned-default late-same-owner-reset)
for EO_INDEX in "${!EO_CASES[@]}"; do
  EO_CASE="${EO_CASES[$EO_INDEX]}"
  /usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
    -Wall -Wextra -Werror -DMCL_EO_SCENARIO="$EO_INDEX" -I "$EO_ROOT" \
    -c "$EO_ROOT/early_bridge.c" -o "$EO_BUILD/$EO_CASE-bridge.o"
  # Direct object arguments force extraction/retention of the constructor object.
  "$EO_RUSTC" --edition 2021 --target x86_64-apple-darwin -O -D warnings \
    "$EO_ROOT/early_probe.rs" -C "link-arg=$EO_BUILD/$EO_CASE-bridge.o" \
    -C "link-arg=$EO_BUILD/native_fault_asm.o" -o "$EO_BUILD/early-owner-$EO_CASE"
  /usr/bin/otool -l "$EO_BUILD/early-owner-$EO_CASE" > "$EO_BUILD/$EO_CASE.macho-loads.txt"
  /usr/bin/otool -tvV "$EO_BUILD/$EO_CASE-bridge.o" > "$EO_BUILD/$EO_CASE.bridge-disassembly.txt"
  /usr/bin/nm -u "$EO_BUILD/$EO_CASE-bridge.o" > "$EO_BUILD/$EO_CASE.undefined.txt"
done
python3 "$EO_ROOT/source_guard.py" --build-info "$EO_BUILD"
