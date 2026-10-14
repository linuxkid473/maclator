#!/bin/bash
# Parent-only serial build recipe. Not invoked by Cargo or source delivery.
set -eu
RA_ROOT="$(cd "$(dirname "$0")" && pwd)"
RA_NATIVE="$RA_ROOT/../native-fault"
RA_BUILD="$RA_ROOT/build"
RA_RUSTC="${RA_RUSTC:-rustc}"
python3 "$RA_ROOT/source_guard.py" --verify
mkdir -p "$RA_BUILD"
"$RA_RUSTC" -vV > "$RA_BUILD/rustc-version.txt"
/usr/bin/clang --version > "$RA_BUILD/clang-version.txt"
/usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
  -Wall -Wextra -Werror -DMCL_RA_OBSERVE=0 -I "$RA_ROOT" \
  -c "$RA_ROOT/adapter_bridge.c" -o "$RA_BUILD/direct_bridge.o"
/usr/bin/clang -arch x86_64 -DMCL_RA_OBSERVE=0 -c "$RA_ROOT/adapter_observer.S" -o "$RA_BUILD/direct_observer.o"
/usr/bin/clang -arch x86_64 -c "$RA_NATIVE/native_fault.S" -o "$RA_BUILD/native_fault_asm.o"
/usr/bin/ar -rcs "$RA_BUILD/libmclra_direct.a" "$RA_BUILD/direct_bridge.o" "$RA_BUILD/direct_observer.o" "$RA_BUILD/native_fault_asm.o"
"$RA_RUSTC" --edition 2021 --target x86_64-apple-darwin -O -D warnings \
  "$RA_ROOT/adapter_probe.rs" -L "native=$RA_BUILD" -l static=mclra_direct -o "$RA_BUILD/adapter-direct-probe"
/usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
  -Wall -Wextra -Werror -DMCL_RA_OBSERVE=1 -I "$RA_ROOT" \
  -c "$RA_ROOT/adapter_bridge.c" -o "$RA_BUILD/observed_bridge.o"
/usr/bin/clang -arch x86_64 -DMCL_RA_OBSERVE=1 -c "$RA_ROOT/adapter_observer.S" -o "$RA_BUILD/observed_observer.o"
/usr/bin/ar -rcs "$RA_BUILD/libmclra_observed.a" "$RA_BUILD/observed_bridge.o" "$RA_BUILD/observed_observer.o" "$RA_BUILD/native_fault_asm.o"
"$RA_RUSTC" --edition 2021 --target x86_64-apple-darwin -O -D warnings \
  "$RA_ROOT/adapter_probe.rs" -L "native=$RA_BUILD" -l static=mclra_observed -o "$RA_BUILD/adapter-observed-probe"
python3 "$RA_ROOT/source_guard.py" --build-info "$RA_BUILD"
