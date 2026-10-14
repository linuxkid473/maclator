#!/bin/bash
# Source prototype build recipe. Not invoked automatically by Cargo.
set -eu
NF_ROOT="$(cd "$(dirname "$0")" && pwd)"
NF_OUT="${NF_OUT:-$NF_ROOT/build}"
mkdir -p "$NF_OUT"
/usr/bin/clang -arch x86_64 -std=c11 -O2 -fno-builtin -fno-stack-protector \
  -Wall -Wextra -Werror -c "$NF_ROOT/native_fault.c" -o "$NF_OUT/native_fault.o"
/usr/bin/clang -arch x86_64 -c "$NF_ROOT/native_fault.S" -o "$NF_OUT/native_fault_asm.o"
/usr/bin/ar -rcs "$NF_OUT/libmclnativefault.a" "$NF_OUT/native_fault.o" "$NF_OUT/native_fault_asm.o"
echo "$NF_OUT/libmclnativefault.a"
